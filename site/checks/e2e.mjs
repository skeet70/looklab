// SPDX-License-Identifier: AGPL-3.0-or-later
// Browser checks: worker round trip, match/permalink, contribute upload with HAR, page screenshots.

import { copyFile, cp, link, mkdir, readFile, writeFile } from 'node:fs/promises';
import { execFileSync } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { fileURLToPath, pathToFileURL } from 'node:url';

const REPO_ROOT = fileURLToPath(new URL('../..', import.meta.url));

const USAGE = `usage: node site/checks/e2e.mjs [options]

  --browser chromium|firefox|webkit
                               engine to drive (default chromium); Firefox and
                               WebKit use Playwright's context credentials, so a
                               WebKit run on a machine without a GPU also needs
                               __EGL_VENDOR_LIBRARY_FILENAMES for Mesa's
                               libEGL_mesa, EGL_PLATFORM=surfaceless and
                               MESA_LOADER_DRIVER_OVERRIDE=swrast
  --flow all|match|contribute|all-bw|pages|flatness|propose|developers|compare|account|lazy
  --origin URL                 stub server origin (default http://localhost:8099)
  --out DIR                    artifact directory (default /tmp/looklab-e2e)
  --state DIR                  stub server state, must match --state (default /tmp/looklab-stub-state)
  --raw FILE --jpeg FILE       the contribution files
  --extra FILE                 one more file in the drop (repeatable)
  --body ID --firmware V --sim KEY
  --controls "key=step,..."    control steps, comma separated
  --bw-all 1                   drop only the eight monochrome X-S10 renders
  --fail-draft-blobs 1         make IndexedDB Blob writes throw, to check the
                               draft-unavailable note and that the flow continues
  --workers N                  worker count for the contribute run
  --verifier FILE              native looklab binary for contribution HAR checks
  --verify-module FILE         served prep module for HAR checks
  --count N                    files for the flatness run (default 100)
  --expected FILE              canonical permalink to compare against
  --status TOKEN               status token for the /status screenshot
  --permalink BASE64URL        pinned match result for the pages flow

needs the fixed-generation launcher and Playwright from nix:

  site/scripts/dev.sh --once

after it serves a generation, keep it running and use another terminal:

  nix develop -c env CARGO_TARGET_DIR="$PWD/target-ui" CARGO_PROFILE_DEV_DEBUG=0 \\
    cargo build --locked --manifest-path target-ui/source/Cargo.toml -p looklab-cli
  nix develop -c env \\
    PLAYWRIGHT_MODULE=$(nix eval --raw nixpkgs#playwright-driver)/index.mjs \\
    PLAYWRIGHT_BROWSERS_PATH=$(nix eval --raw nixpkgs#playwright-driver.browsers) \\
    node site/checks/e2e.mjs --browser chromium --flow all --origin http://localhost:8081 \\
      --out /tmp/looklab-e2e --state target-ui/stub-state`;

const options = {
  browser: 'chromium',
  flow: 'all',
  origin: 'http://localhost:8099',
  out: '/tmp/looklab-e2e',
  state: '/tmp/looklab-stub-state',
  raw: 'fixtures/cache/fujifilm-x-s10.raf',
  jpeg: 'fixtures/generated/fujifilm-x-s10.jpg',
  extra: [],
  body: 'fujifilm/x-s10',
  firmware: '1.00',
  sim: 'provia',
  controls: '',
  workers: 1,
  verifier: 'target-ui/debug/looklab',
  'verify-module': 'target-ui/dist/generated/wasm/looklab_prep_module.wasm',
  count: 100,
  expected: 'target-ui/dist/data/looks/synthetic-match/1.0.0/match.permalink',
  status: '',
  permalink: '',
  'fail-draft-blobs': false,
  'bw-all': false,
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
  options[key] = ['workers', 'count'].includes(key)
    ? Number(value)
    : ['fail-draft-blobs', 'bw-all'].includes(key)
      ? value === '1' || value === 'true'
      : key === 'extra'
        ? [...options.extra, value]
        : value;
}

const playwrightModule = process.env.PLAYWRIGHT_MODULE;
const playwright = playwrightModule
  ? await import(pathToFileURL(playwrightModule).href)
  : await import('playwright-core');
const { chromium, firefox, webkit } = playwright.default ?? playwright;
const ENGINES = { chromium, firefox, webkit };
const engine = ENGINES[options.browser];
if (!engine) {
  throw new Error(`unknown browser ${options.browser}\n\n${USAGE}`);
}

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

const BW_MAKERNOTE_VARIANTS = [
  ['acros', 0x0500],
  ['acros-r', 0x0501],
  ['acros-ye', 0x0502],
  ['acros-g', 0x0503],
  ['monochrome', 0x0300],
  ['monochrome-r', 0x0301],
  ['monochrome-ye', 0x0302],
  ['monochrome-g', 0x0303],
];

/// A copy of the fixture JPEG whose Fujifilm makernote sets `tag` to `value`.
async function renderWithTag(name, tag, value) {
  const path = `${options.out}/${name}.jpg`;
  const bytes = await readFile(options.jpeg);
  let index = 2;
  let tiffStart = -1;
  while (index + 4 <= bytes.length) {
    if (bytes[index] !== 0xff) break;
    const marker = bytes[index + 1];
    if (marker === 0xda || marker === 0xd9) break;
    const length = bytes.readUInt16BE(index + 2);
    if (marker === 0xe1 && bytes.toString('latin1', index + 4, index + 10) === 'Exif\0\0') {
      tiffStart = index + 10;
      break;
    }
    index += 2 + length;
  }
  if (tiffStart < 0) throw new Error('the fixture JPEG has no EXIF segment');
  const patched = Buffer.from(bytes);
  const little = patched.toString('latin1', tiffStart, tiffStart + 2) === 'II';
  const u16 = (at) => (little ? patched.readUInt16LE(at) : patched.readUInt16BE(at));
  const u32 = (at) => (little ? patched.readUInt32LE(at) : patched.readUInt32BE(at));
  const ifd0 = tiffStart + u32(tiffStart + 4);
  const count0 = u16(ifd0);
  let exifIfd = null;
  for (let entry = 0; entry < count0; entry += 1) {
    const at = ifd0 + 2 + entry * 12;
    if (u16(at) === 0x8769) exifIfd = tiffStart + u32(at + 8);
  }
  const ifd = exifIfd ?? ifd0;
  const count = u16(ifd);
  for (let entry = 0; entry < count; entry += 1) {
    const at = ifd + 2 + entry * 12;
    if (u16(at) !== 0x927c || u16(at + 2) !== 7) continue;
    const note = tiffStart + u32(at + 8);
    if (patched.toString('latin1', note, note + 8) !== 'FUJIFILM') {
      throw new Error('the MakerNote is not Fujifilm');
    }
    const startOf = patched.readUInt32LE(note + 8);
    const entries = patched.readUInt16LE(note + startOf);
    for (let field = 0; field < entries; field += 1) {
      const fieldAt = note + startOf + 2 + field * 12;
      if (patched.readUInt16LE(fieldAt) !== tag) continue;
      if (little) patched.writeUInt16LE(value, fieldAt + 8);
      else patched.writeUInt16BE(value, fieldAt + 8);
      await writeFile(path, patched);
      return path;
    }
    throw new Error(`the Fujifilm makernote has no tag ${tag.toString(16)}`);
  }
  throw new Error('the EXIF has no MakerNote');
}

async function addVirtualAuthenticator(page) {
  if (options.browser !== 'chromium') {
    await page.context().credentials.install();
    return null;
  }
  const session = await page.context().newCDPSession(page);
  await session.send('WebAuthn.enable');
  await session.send('Emulation.setFocusEmulationEnabled', { enabled: true });
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
  return { cdp: session, authenticatorId };
}

async function open(viewport, harPath) {
  const browser = await engine.launch();
  const context = await browser.newContext({
    viewport,
    acceptDownloads: true,
    ...(harPath
      ? {
          recordHar: {
            path: harPath,
            content: 'embed',
            urlFilter: /\/api\/(nonce|passkey\/[^/?]+|session(?:\/[^/?]+)?|upload)|\/status\/[A-Za-z0-9_-]+$/,
          },
        }
      : {}),
  });
  const page = await context.newPage();
  await page.addInitScript(() => {
    window.__looklabWebauthnGets = 0;
    const proto = CredentialsContainer.prototype;
    const get = proto.get;
    proto.get = function (...args) {
      window.__looklabWebauthnGets = (window.__looklabWebauthnGets ?? 0) + 1;
      return get.apply(this, args);
    };
  });
  if (options['fail-draft-blobs']) {
    await page.addInitScript(() => {
      const put = IDBObjectStore.prototype.put;
      IDBObjectStore.prototype.put = function (value, key) {
        if (value instanceof Blob) {
          throw new Error('draft blob write refused');
        }
        return put.call(this, value, key);
      };
    });
  }
  await page.bringToFront();
  // The dev loop reloads when its build stamp changes; an empty stamp keeps a concurrent UI edit from replacing the document.
  await page.route('**/dev-build-stamp.txt', route => route.fulfill({ status: 200, contentType: 'text/plain', body: '' }));
  const bodies = new Map();
  page.on('request', (request) => {
    if (!/\/api\/|\/status\//.test(request.url())) {
      return;
    }
    const data = request.postDataBuffer();
    if (data) {
      const key = `${request.method()} ${request.url()}`;
      const list = bodies.get(key) ?? [];
      list.push(data.toString('base64'));
      bodies.set(key, list);
    }
  });
  const problems = [];
  const expectedMissingResources = new Set();
  page.on('pageerror', (error) => problems.push(String(error)));
  page.on('console', (message) => {
    const expectedNetworkFailure = expectedMissingResources.has(message.location().url) && message.args().length === 0;
    if (message.type() === 'error' && !expectedNetworkFailure) {
      const detail = `${message.text()} (${message.location().url})`;
      problems.push(detail.replace(/(\/status\/)[A-Za-z0-9_-]+(?:\.rss)?/g, '$1[private]'));
    }
  });
  const authenticator = await addVirtualAuthenticator(page);
  return { browser, context, page, problems, bodies, authenticator, expectedMissingResources };
}

async function close(session) {
  await session.context.close();
  await session.browser.close();
}

async function match(size) {
  const session = await open(VIEWPORTS[size]);
  const { page, problems } = session;
  try {
    if (options.browser === 'chromium') {
      const cdp = await session.context.newCDPSession(page);
      await cdp.send('Network.setCacheDisabled', { cacheDisabled: true });
      await session.context.grantPermissions(['clipboard-read', 'clipboard-write']);
    }
    await page.goto(`${options.origin}/match?camera=fujifilm/x-t5&look=sunlit-negative`);
    const recipe = page.getByRole('article').filter({ has: page.getByRole('heading', { name: 'Take this recipe shooting' }) });
    await recipe.or(page.getByRole('alert')).first().waitFor({ timeout: 120_000 });
    if (await page.getByRole('alert').count()) {
      throw new Error(`Recipe failed: ${await page.getByRole('alert').allTextContents()}`);
    }
    await recipe.waitFor({ timeout: 120_000 });
    await page.getByRole('heading', { name: 'Sunlit Negative on your Fujifilm X-T5', exact: true }).waitFor();
    await page.getByRole('slider', { name: 'Comparison divider' }).waitFor({ timeout: 120_000 });
    const prefill = new URL(page.url());
    check(prefill.searchParams.get('camera') === 'fujifilm/x-t5' && prefill.searchParams.get('look') === 'sunlit-negative', `${size}: camera and look stay prefilled from the URL`);
    check(await page.getByRole('heading', { name: 'Which look?', exact: true }).count() === 0, `${size}: a prefilled URL lands on the result`);
    check(await page.getByText('Left: target look', { exact: true }).count() === 1, `${size}: the result splits target from realization`);
    check(await page.getByText('Right: your camera at these settings', { exact: true }).count() === 1, `${size}: the realization side is labelled`);
    const closeness = recipe.locator('.closeness-label');
    check(await closeness.count() === 1, `${size}: the result carries a closeness label`);
    check(!(await recipe.locator('.closeness-details').isVisible()), `${size}: closeness numbers wait to be asked for`);
    await closeness.click();
    check(await recipe.locator('.closeness-details').isVisible(), `${size}: closeness numbers appear on demand`);
    const settings = await recipe.locator('dl').innerText();
    const share = page.getByRole('link', { name: 'Share link', exact: true });
    const href = await share.getAttribute('href');
    const permalink = Buffer.from(href.split('/').pop(), 'base64url').toString('utf8');
    if (options.browser === 'chromium') {
      await page.getByRole('button', { name: 'Copy as text', exact: true }).focus();
      await page.keyboard.press('Enter');
      await page.getByRole('status').filter({ hasText: 'Settings copied' }).waitFor();
      const copied = await page.evaluate(() => navigator.clipboard.readText());
      check(copied.includes('Film simulation:'), `${size}: copied settings are readable`);
      const exposure = copied.split('\n').find(line => line.startsWith('Exposure compensation:'));
      check(/^Exposure compensation: [+-](?:\d+|\d+\/3) EV$/.test(exposure ?? ''), `${size}: exposure compensation uses exact thirds: ${exposure}`);
    }
    for (const colorScheme of ['dark', 'light']) {
      await page.emulateMedia({ colorScheme });
      check(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), `${size}: match fits viewport`);
      await page.screenshot({ path: `${options.out}/match-${size}-${colorScheme}.png`, fullPage: true });
    }
    await share.click();
    await recipe.or(page.getByRole('alert')).first().waitFor({ timeout: 120_000 });
    if (await page.getByRole('alert').count()) {
      throw new Error(`Pinned recipe failed: ${await page.getByRole('alert').allTextContents()}`);
    }
    await recipe.waitFor({ timeout: 600_000 });
    await page.getByRole('slider', { name: 'Comparison divider' }).waitFor({ timeout: 120_000 });
    check(await recipe.locator('dl').innerText() === settings, `${size}: permalink reloads the same settings`);
    check(await page.getByRole('link', { name: 'Share link', exact: true }).getAttribute('href') === href, `${size}: pinned result has the same permalink`);
    await page.getByText('Check with your shots', { exact: true }).click();
    await page.locator('.match-verification input[type=file]').setInputFiles({ name: 'invalid.jpg', mimeType: 'image/jpeg', buffer: Buffer.from('not a JPEG') });
    await page.getByRole('alert').filter({ hasText: 'Choose JPEGs' }).waitFor();
    const jpeg = await readFile(options.jpeg);
    await page.locator('.match-verification .drop-zone').evaluate((zone, encoded) => {
      const transfer = new DataTransfer();
      transfer.items.add(new File([Uint8Array.from(atob(encoded), character => character.charCodeAt(0))], 'shot.jpg', { type: 'image/jpeg' }));
      zone.dispatchEvent(new DragEvent('drop', { dataTransfer: transfer, bubbles: true, cancelable: true }));
    }, jpeg.toString('base64'));
    await page.getByRole('alert').filter({ hasText: 'Add the original RAW files' }).waitFor({ timeout: 120_000 });
    await page.screenshot({ path: `${options.out}/match-verification-${size}.png`, fullPage: true });
    await page.goto(`${options.origin}/match`);
    const cameras = page.getByRole('combobox', { name: 'Your camera', exact: true });
    await cameras.waitFor();
    await cameras.focus();
    await page.keyboard.press('Home');
    await page.keyboard.press('ArrowDown');
    await page.keyboard.press('Enter');
    const firstNext = page.getByRole('button', { name: 'Next', exact: true });
    const cameraPicker = await cameras.boundingBox();
    check((await firstNext.boundingBox()).y > cameraPicker.y + cameraPicker.height, `${size}: wizard navigation follows camera controls`);
    for (let attempt = 0; attempt < 300; attempt += 1) {
      if (await firstNext.isEnabled()) break;
      await page.waitForTimeout(100);
    }
    check(await firstNext.isEnabled(), `${size}: wizard enables Next once the camera settings load`);
    await page.getByRole('button', { name: 'Next', exact: true }).focus();
    await page.keyboard.press('Enter');
    await page.getByRole('heading', { name: 'Which look?', exact: true }).waitFor();
    await page.getByRole('button', { name: 'Next', exact: true }).focus();
    await page.keyboard.press('Enter');
    await page.getByRole('heading', { name: 'How will you apply it?', exact: true }).waitFor();
    await page.screenshot({ path: `${options.out}/match-wizard-${size}.png`, fullPage: true });
    await page.getByRole('button', { name: 'Get my settings', exact: true }).focus();
    await page.keyboard.press('Enter');
    check(!(await page.getByRole('button', { name: 'Back', exact: true }).isEnabled()), `${size}: choices stay locked while computing a match`);
    await recipe.waitFor({ timeout: 600_000 });
    check(problems.length === 0, `${options.browser} ${size}: no page errors during match: ${problems.join('; ')}`);
    await page.getByRole('button', { name: 'Change choices', exact: true }).click();
    await page.getByRole('button', { name: 'Back', exact: true }).click();
    await page.getByRole('button', { name: 'Back', exact: true }).click();
    let cameraRequestReady;
    const cameraRequest = new Promise(resolve => { cameraRequestReady = resolve; });
    await page.route('**/data/bodies/fujifilm/x-m1.json', route => cameraRequestReady(route));
    const changeCamera = async model => {
      await cameras.fill(model);
      await page.keyboard.press('ArrowDown');
      await page.keyboard.press('Enter');
    };
    await changeCamera('X-M1');
    const pendingCamera = await cameraRequest;
    check(!(await page.getByRole('button', { name: 'Next', exact: true }).isEnabled()), `${size}: camera changes wait for the new settings`);
    await pendingCamera.fulfill({ status: 503, body: 'Unavailable for this browser check' });
    await page.getByRole('alert').filter({ hasText: 'Your camera settings could not load.' }).waitFor();
    check(!(await page.getByRole('button', { name: 'Next', exact: true }).isEnabled()), `${size}: camera load failure cannot submit old settings`);
    await page.screenshot({ path: `${options.out}/match-camera-error-${size}.png`, fullPage: true });
    await page.unroute('**/data/bodies/fujifilm/x-m1.json');
    await page.route(/\/scenes\/photo-.*-thumb\.png$/, route => route.fulfill({ status: 503, body: 'Unavailable for this browser check' }));
    await page.goto(`${options.origin}/match`);
    await cameras.waitFor();
    await changeCamera('X-T5');
    await page.getByRole('button', { name: 'Next', exact: true }).click();
    await page.getByRole('status').filter({ hasText: 'Look thumbnails could not load.' }).waitFor();
    await page.getByRole('button', { name: 'Choose a look', exact: true }).click();
    await page.getByRole('dialog', { name: 'Choose a look', exact: true }).getByRole('button', { name: /Cold Steel/ }).click();
    await page.getByText('Cold Steel', { exact: true }).filter({ visible: true }).waitFor();
    check(await page.getByRole('button', { name: 'Choose a look', exact: true }).getAttribute('aria-expanded') === 'false', `${size}: looks stay selectable without thumbnails`);
    await page.screenshot({ path: `${options.out}/match-thumbnail-error-${size}.png`, fullPage: true });
    return permalink;
  } catch (error) {
    console.error(`Match page at failure: ${await page.locator('body').innerText()}`);
    console.error(`Match browser diagnostics: ${problems.join('; ')}`);
    await page.screenshot({ path: `${options.out}/match-failed-${size}.png`, fullPage: true });
    throw error;
  } finally {
    await close(session);
  }
}

async function contribute(size) {
  const allBw = options['bw-all'];
  const harPath = `${options.out}/session-${size}.har`;
  const session = await open(VIEWPORTS[size], harPath);
  const { page, problems } = session;
  if (session.authenticator) {
    await session.authenticator.cdp.send('Network.enable');
    await session.authenticator.cdp.send('Network.setCacheDisabled', { cacheDisabled: true });
  }
  const summary = {};
  const capture = async (name, fullPage = true) => {
    for (const colorScheme of ['light', 'dark']) {
      await page.emulateMedia({ colorScheme });
      await page.screenshot({ path: `${options.out}/${name}-${size}-${colorScheme}.png`, fullPage });
      check(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth && [...document.querySelectorAll('dialog[open]')].every(dialog => dialog.scrollWidth <= dialog.clientWidth)), `${size} ${colorScheme}: ${name} fits viewport`);
    }
    await page.emulateMedia({ colorScheme: 'light' });
  };
  try {
    await page.goto(`${options.origin}/contribute`);
    await page.getByRole('heading', { name: 'Help build looks', exact: true }).waitFor();
    await capture('contribute-landing');
    const catalog = await (await page.request.get(`${options.origin}/data/index.json`)).json();
    const wantedLook = catalog.looks.find(look => look.visibility !== 'test_fixture');
    const wantedCamera = catalog.bodies.find(camera => camera.kind === 'camera');
    check(Boolean(wantedCamera), `${size}: wanted-shot fixture uses a published camera`);
    const wantedBounties = wantedLook ? [0.01, 0.1, 0.5, 0.9].map((reduction, index) => ({
      look: wantedLook.slug,
      look_version: 1,
      body: wantedCamera.id,
      region: `h${index}/e3`,
      expected_variance_reduction: reduction,
      reason: { kind: 'unverified_region', sigma: 0.2 },
      status: 'open',
    })) : [];
    await page.route('**/bounties.json', route => route.fulfill({ json: { bounties: wantedBounties } }));
    const guideNames = [
      'Share photos you already have',
      'Take a few wanted shots',
      'Re-render a few RAWs in camera',
      'Photograph a pattern on your phone',
      'Photograph a gradient',
      'Photograph a color chart',
    ];
    for (const [index, name] of guideNames.entries()) {
      const card = page.getByRole('button', { name, exact: true });
      await card.focus();
      await page.keyboard.press('Enter');
      const dialog = page.getByRole('dialog', { name, exact: true });
      await dialog.waitFor();
      check(await dialog.getByRole('list').first().getByRole('listitem').count() >= 3, `${size}: ${name} has numbered guidance`);
      if (index === 1) {
        if (wantedLook) {
          await dialog.getByRole('article').nth(2).waitFor();
          check(await dialog.getByRole('article').count() === 3, `${size}: wanted guide shows the top three requests`);
          check(await dialog.getByRole('article').first().textContent().then(text => text.includes(wantedLook.name) && text.includes(`${wantedCamera.make} ${wantedCamera.model}`)), `${size}: wanted guide shows published look and camera names`);
        } else {
          await dialog.getByRole('heading', { name: 'No wanted shots right now', exact: true }).waitFor();
          check(await dialog.getByRole('article').count() === 0, `${size}: a catalog without looks has no wanted-shot requests`);
        }
      }
      if (index === 3) {
        const { createHash } = await import('node:crypto');
        const fixtureSeed = createHash('sha256').update('looklab-fixture-pattern-v1').digest('hex');
        const link = dialog.getByRole('link', { name: 'Open the measurement pattern on your phone', exact: true });
        const address = await dialog.getByRole('textbox', { name: 'Pattern URL', exact: true }).inputValue();
        const expected = new URL(`pattern/${fixtureSeed}`, `${options.origin}/`).href;
        check(address === expected, `${size}: phone link uses the public fixture and deployment prefix`);
        check(await link.evaluate(anchor => anchor.href) === address, `${size}: phone link and selectable URL agree`);
        await writeFile(`${options.out}/pattern-${size}.url`, `${address}\n`);
        await dialog.getByRole('img', { name: 'Scan to open the measurement pattern', exact: true }).screenshot({ path: `${options.out}/pattern-qr-${size}.png` });
        const [pattern] = await Promise.all([page.waitForEvent('popup'), link.click()]);
        try {
          if (options.browser === 'chromium') {
            const cdp = await page.context().newCDPSession(pattern);
            await cdp.send('Network.setCacheDisabled', { cacheDisabled: true });
          }
          await pattern.reload();
          const canvas = pattern.getByRole('button', { name: 'Measurement pattern. Tap or press Enter for fullscreen.', exact: true });
          await canvas.waitFor();
          check(pattern.url() === address, `${size}: phone link opens its pattern in a new tab`);
          const rendered = () => pattern.locator('canvas').evaluate(canvas => canvas.width === innerWidth && canvas.height === innerHeight && canvas.getContext('2d').getImageData(0, 0, canvas.width, canvas.height).data.some((channel, index) => index % 4 !== 3 && channel !== 0));
          check(await rendered(), `${size}: phone pattern fills the viewport with rendered pixels`);
          check(!await pattern.locator('.site-header').isVisible() && !await pattern.locator('footer').isVisible(), `${size}: phone pattern has no site chrome`);
          for (const colorScheme of ['light', 'dark']) {
            await pattern.emulateMedia({ colorScheme });
            await pattern.screenshot({ path: `${options.out}/phone-pattern-${size}-${colorScheme}.png` });
          }
          if (await pattern.evaluate(() => document.fullscreenEnabled)) {
            await canvas.focus();
            await pattern.keyboard.press('Enter');
            await pattern.waitForFunction(() => document.fullscreenElement !== null);
            check(true, `${size}: pattern supports keyboard fullscreen`);
            await pattern.evaluate(() => document.exitFullscreen());
          }
          const other = new URL(address);
          other.pathname = other.pathname.replace(/[a-f0-9]{64}$/, 'ab'.repeat(32));
          await pattern.goto(other.href);
          await canvas.waitFor();
          check(await rendered(), `${size}: pattern route renders another 64-hex seed`);
        } finally {
          await pattern.close();
        }
      }
      await capture(`contribute-help-${index}`, false);
      await page.keyboard.press('Escape');
      await dialog.waitFor({ state: 'hidden' });
      check(await card.evaluate(button => button === document.activeElement), `${size}: ${name} returns keyboard focus`);
    }
    await page.getByRole('button', { name: guideNames[0], exact: true }).click();
    const [chooser] = await Promise.all([
      page.waitForEvent('filechooser'),
      page.getByRole('dialog', { name: guideNames[0], exact: true }).getByRole('button', { name: 'Choose photos', exact: true }).click(),
    ]);
    const bwRenders = allBw
      ? await Promise.all(BW_MAKERNOTE_VARIANTS.map(([name, value]) =>
          renderWithTag(`render-${name}`, 0x1003, value)))
      : [];
    const toneVariant = allBw ? null : await renderWithTag('render-provia-highlight', 0x1041, 0x0010);
    const acros = allBw ? null : await renderWithTag('render-acros', 0x1003, 0x0500);
    const acrosYellow = allBw ? null : await renderWithTag('render-acros-ye', 0x1003, 0x0502);
    const sepia = allBw ? null : await renderWithTag('render-sepia', 0x1003, 0x0310);
    const dropped = allBw
      ? [options.raw, ...bwRenders]
      : [
          options.raw,
          options.jpeg,
          toneVariant,
          acros,
          acrosYellow,
          sepia,
          ...options.extra,
        ];
    summary.files = allBw
      ? [options.raw, ...bwRenders]
      : [
          options.raw,
          options.jpeg,
          toneVariant,
          acros,
          acrosYellow,
          sepia,
        ];
    await chooser.setFiles(dropped);
    await page.getByRole('heading', { name: /^\d+ pairs? found$/ }).waitFor({ timeout: 120_000 });
    await capture('contribute-1-drop');
    const stepItems = page.locator('.stepper .step-item');
    check(await stepItems.count() === 6, `${size}: the stepper lists six steps`);
    check(await stepItems.first().getAttribute('data-state') === 'complete', `${size}: the drop step is marked complete`);
    check(await stepItems.nth(1).getAttribute('aria-current') === 'step', `${size}: the current step carries aria-current="step"`);
    check(await stepItems.nth(2).getAttribute('data-state') === 'upcoming', `${size}: later steps are marked upcoming`);
    const firstSummary = (await stepItems.first().locator('.step-summary').textContent()).trim();
    const pairCount = Number((await page.getByRole('heading', { name: /^\d+ pairs? found$/ }).textContent()).trim().split(' ')[0]);
    check(firstSummary.includes(`${pairCount} ${pairCount === 1 ? 'pair' : 'pairs'}`), `${size}: a completed step keeps its summary (${JSON.stringify(firstSummary)})`);
    const describedBy = await stepItems.first().locator('.step-link').getAttribute('aria-describedby');
    check(Boolean(describedBy) && await page.locator(`#${describedBy}`).count() === 1, `${size}: a completed step is described by its summary`);
    check(await stepItems.nth(2).locator('.step-summary').count() === 0, `${size}: an upcoming step has no summary`);
    if (options['fail-draft-blobs']) {
      await page.getByText("This browser couldn't keep a local draft").waitFor({ timeout: 30_000 });
    }
    const draftUnavailable = await page.getByText("This browser couldn't keep a local draft").count() > 0;
    if (options['fail-draft-blobs']) {
      check(draftUnavailable, `${size}: a failed draft save is reported without blocking`);
    }
    summary.draft_unavailable = draftUnavailable;

    if (!allBw) {
      const colourBatches = page.locator('.batch-choice').filter({
        has: page.locator('.batch-choice-title', { hasText: /^Batch \d+: \d+ photos?, Provia$/ }),
      });
      check(await colourBatches.count() === 2, `${size}: matching camera, firmware and sim with different settings form separate groups`);
      const highlightSteps = [];
      for (const batch of await colourBatches.all()) {
        await batch.click();
        highlightSteps.push(await page.getByRole('spinbutton', { name: /^highlight tone$/i }).inputValue());
      }
      check(highlightSteps.includes('0') && highlightSteps.includes('-2'), `${size}: each group retains its own recorded highlight setting`);
    }
    await page.locator('.batch-choice').filter({
      has: page.locator('.batch-choice-title', { hasText: new RegExp(`^Batch \\d+: \\d+ photos?, ${allBw ? 'Acros' : 'Provia'}$`) }),
    }).first().click();
    await page.getByRole('textbox', { name: 'Camera', exact: true }).fill(options.body);
    await page.getByRole('textbox', { name: 'Firmware', exact: true }).fill(options.firmware);
    if (!allBw) {
      await page.getByRole('textbox', { name: 'Film simulation', exact: true }).fill(options.sim);
      await page.getByRole('checkbox', { name: 'Settings contribution' }).check();
      await page.getByLabel('Look these settings are trying to match', { exact: true }).fill('cold-steel');
      await page.getByLabel('Look these settings are trying to match', { exact: true }).fill('sunlit-negative');
      await page.getByRole('checkbox', { name: 'Settings contribution' }).uncheck();
    }
    await page.click('summary:has-text("Technical settings editor")');
    if (options.controls) {
      await page.getByLabel('Settings (key=step, one per line)', { exact: true }).fill(options.controls.split(',').join('\n'));
    }
    await page.click('summary:has-text("Processing options")');
    await page.selectOption('#workers', String(options.workers));
    const consent = page.locator('.monochrome-consent');
    await consent.waitFor();
    const consentBox = consent.getByRole('checkbox', {
      name: 'Contribute all B&W renders in this selection for monochrome model development',
    });
    check(!(await consentBox.isChecked()), `${size}: drop-level B&W consent starts unchecked`);
    const promise = await consent.innerText();
    check(
      promise.includes('180 days') && promise.includes('withdraw') && promise.includes('No published Look is promised'),
      `${size}: B&W consent states purpose, retention, withdrawal and no Look promise`,
    );
    const bwLabels = allBw
      ? ['Acros', 'Acros R', 'Acros Ye', 'Acros G', 'Monochrome', 'Monochrome R', 'Monochrome Ye', 'Monochrome G']
      : ['Acros', 'Acros Ye', 'Sepia'];
    const declarations = new Set();
    for (const label of bwLabels) {
      const batch = page.locator('.batch-choice').filter({
        has: page.locator('.batch-choice-title', {
          hasText: new RegExp(`^Batch \\d+: \\d+ photos?, ${label}$`),
        }),
      });
      await batch.waitFor();
      await batch.click();
      check(
        (await page.locator('.batch-editor').innerText()).includes('Normalized family:'),
        `${size}: ${label} shows the normalized family beside the native sim`,
      );
      if (!allBw && label === 'Acros Ye') {
        check(
          (await page.locator('.batch-editor').innerText()).includes('Normalized monochrome filter: Yellow'),
          `${size}: Acros Ye keeps its normalized filter`,
        );
      }
      const unmappedLabels = page.locator('.unmapped-controls .menu-setting');
      for (let index = 0; index < await unmappedLabels.count(); index += 1) {
        const field = unmappedLabels.nth(index);
        const key = (await field.textContent()).trim().replaceAll(' ', '_');
        await field.locator('input').fill('0');
        declarations.add(key);
      }
    }
    const progress = page.getByRole('navigation', { name: 'Progress' });
    if (!allBw) {
      await page.getByRole('button', { name: 'Process locally', exact: true }).click();
      await page.getByRole('heading', { name: /Review what/ }).waitFor({ timeout: 900_000 });
      const withoutConsent = await page.locator('.payload-summary').textContent();
      check(withoutConsent.includes('2 colour renders') && !withoutConsent.includes('B&W'), `${size}: preparation leaves out B&W without consent`);
      check(await page.locator('.step:not([hidden]) .excluded .excluded-files li').count() === 3, `${size}: the review lists the three B&W renders left out`);
      await page.getByRole('button', { name: 'Continue to send', exact: true }).click();
      await progress.getByRole('button', { name: 'Check groups', exact: true }).click();
    }
    await consent.waitFor();
    check(!(await consentBox.isChecked()), `${size}: B&W consent stays unchecked until an explicit click`);
    await consentBox.check();
    check(await consentBox.isChecked(), `${size}: one click selects every B&W render in the drop`);
    await capture('contribute-consent-changed');
    check(await progress.getByRole('button', { name: 'Review', exact: true }).isDisabled(), `${size}: changed B&W selection requires fresh preparation before review`);
    check(await progress.getByRole('button', { name: 'Send', exact: true }).isDisabled(), `${size}: changed B&W selection cannot reuse the previous send step`);
    if (!draftUnavailable) {
      await page.getByRole('link', { name: 'looklab', exact: true }).click();
      await page.waitForURL((url) => url.pathname === '/');
      await page.goto(`${options.origin}/contribute`);
      await page.getByRole('heading', { name: /^\d+ pairs? found$/ }).waitFor({ timeout: 30_000 });
      check(await progress.getByRole('button', { name: 'Review', exact: true }).isDisabled(), `${size}: a restored changed selection still needs preparation`);
      await consent.waitFor();
      check(await consentBox.isChecked(), `${size}: B&W consent survives draft restoration`);
    }

    summary.declarations = [...declarations];
    await capture('contribute-2-confirm');

    const prepModule = '**/generated/wasm/looklab_prep_module.wasm';
    const slowPrep = async (route) => {
      await new Promise((resolve) => setTimeout(resolve, 3000));
      await route.continue();
    };
    await page.route(prepModule, slowPrep);
    const started = Date.now();
    await page.click('button:has-text("Process locally")');
    await page.getByText(/Processing photo \d+ of \d+/).waitFor({ timeout: 60_000 });
    await capture('contribute-processing');
    await page.waitForSelector('h2:has-text("Review what")', { timeout: 900_000 });
    await page.unroute(prepModule, slowPrep);
    summary.prep_millis = Date.now() - started;
    await capture('contribute-3-inspector');
    check((await page.locator('.payload-summary').textContent()).includes('No pixels, no location'), `${size}: upload summary explains privacy`);
    check(await page.locator('details.excluded-files').count() === 0, `${size}: every selected render was prepared`);
    const withConsent = await page.locator('.payload-summary').textContent();
    if (allBw) {
      check(withConsent.includes('8 B&W renders') && !withConsent.includes('colour renders'), `${size}: the inspector counts eight B&W renders and no colour renders`);
    } else {
      check(withConsent.includes('2 colour renders and 3 B&W renders'), `${size}: the inspector counts colour and B&W renders separately`);
    }
    await page.locator('summary:has-text("See exactly")').focus();
    await page.keyboard.press('Enter');
    await capture('contribute-inspector-expanded');

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

    const [payloadDownload] = await Promise.all([
      page.waitForEvent('download'),
      page.click('button:has-text("Download payload")'),
    ]);
    await payloadDownload.saveAs(`${options.out}/payload-${size}.json`);
    const payload = JSON.parse(await readFile(`${options.out}/payload-${size}.json`, 'utf8'));
    const metadataKeys = payload.images.flatMap((image) => Object.keys(image.metadata ?? {}));
    check(!metadataKeys.some((key) => /seed|authorization|source|makernote_blob/i.test(key)), `${size}: the inspector adds no consent, seed or source data`);
    check(
      payload.images.every((image) => Object.keys(image).sort().join(',') === 'blob_bytes,metadata,removed,rerender_group'),
      `${size}: the inspector payload keeps its existing shape`,
    );
    if (!draftUnavailable) {
      const beforeResume = await page.locator('.payload-summary').textContent();
      await page.reload();
      await page.waitForSelector('h2:has-text("Review what")');
      check(await page.locator('.payload-summary').textContent() === beforeResume, `${size}: local draft resumes prepared samples`);
      if (!allBw) {
        await progress.getByRole('button', { name: 'Check groups', exact: true }).click();
        await page.locator('.batch-choice').filter({
          has: page.locator('.batch-choice-title', { hasText: /^Batch \d+: \d+ photos?, Provia$/ }),
        }).first().click();
        check(await page.getByLabel('Look these settings are trying to match', { exact: true }).inputValue() === 'sunlit-negative', `${size}: latest settings survive draft resume`);
        await progress.getByRole('button', { name: 'Review', exact: true }).click();
      }
    }

    await page.bringToFront();
    await page.click('button:has-text("Continue to send")');
    await capture('contribute-send');
    const [seedDownload] = await Promise.all([
      page.waitForEvent('download'),
      page.click('button:has-text("Export seed file")'),
    ]);
    summary.seeds = `${options.out}/seeds-${size}.json`;
    await seedDownload.saveAs(summary.seeds);
    await page.getByRole('button', { name: 'Sign in', exact: true }).first().click();
    await page.getByRole('button', { name: 'Create a profile', exact: true }).click();
    await page
      .getByRole('dialog', { name: 'Sign in to looklab' })
      .waitFor({ state: 'hidden', timeout: 60_000 });
    await page.locator('.account-menu summary').waitFor({ timeout: 60_000 });
    await page.evaluate(() => { window.__looklabWebauthnGets = 0; });
    const uploads = [];
    const onUpload = (response) => {
      if (response.request().method() === 'POST' && new URL(response.url()).pathname === '/api/upload') {
        uploads.push(response);
      }
    };
    page.on('response', onUpload);
    await page.click('button:has-text("Send contribution")');
    await page.getByRole('heading', { name: 'Thanks, your samples are in', exact: true }).waitFor({ timeout: 300_000 });
    page.off('response', onUpload);
    const expectedBatches = allBw ? 8 : 5;
    check(uploads.length === expectedBatches, `${size}: all ${expectedBatches} batches were uploaded`);
    const uploadBodies = await Promise.all(uploads.map(async (response) => {
      check(response.status() === 201, `${size}: every batch was accepted`);
      return response.json();
    }));
    const contributionId = uploadBodies[0].contribution_id;
    check(Boolean(contributionId), `${size}: every batch returns a contribution id`);
    const gets = await page.evaluate(() => window.__looklabWebauthnGets);
    check(gets === 1, `${size}: exactly one WebAuthn get for ${expectedBatches} batches (${gets})`);
    await capture('contribute-4-upload');
    const done = page.locator('.contribution-done');
    const doneText = await done.textContent();
    check(!/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/i.test(doneText), `${size}: the Done view shows no contribution ids`);
    check(await done.getByRole('link', { name: 'Your contributions', exact: true }).isVisible(), `${size}: the Done view links to Your contributions`);
    check(await page.locator('.stepper-actions').count() === 0, `${size}: Back and Next are gone after the send`);
    check(await stepItems.nth(5).getAttribute('aria-current') === 'step', `${size}: Done is the current step`);
    check(await page.locator('.stepper .step-item[data-state=complete]').count() === 5, `${size}: five steps are marked complete`);
    check(await page.getByRole('navigation', { name: 'Progress' }).getByRole('button', { name: 'Send', exact: true }).isDisabled(), `${size}: completed batch cannot reopen Send`);
    check(problems.length === 0, `${options.browser} ${size}: no page errors during contribute`);

    const feed = page.waitForResponse(
      (response) =>
        response.request().method() === 'GET' &&
        /^\/status\/[A-Za-z0-9_-]+$/.test(new URL(response.url()).pathname),
      { timeout: 60_000 },
    );
    await page.getByRole('main').getByRole('link', { name: 'Your contributions', exact: true }).click();
    await page.waitForURL((url) => url.pathname.endsWith('/me'), { timeout: 60_000 });
    await page.getByRole('heading', { name: 'Your contributions', exact: true }).waitFor({ timeout: 60_000 });
    const feedResponse = await feed;
    check(feedResponse.ok(), `${size}: contribution feed loads`);
    const feedBody = await feedResponse.json();
    check(Array.isArray(feedBody.activities), `${size}: contribution feed returns activities`);
    check(feedBody.activities.length === 1 && feedBody.activities[0].source === 'send', `${size}: one send reads as one submission`);
    const contributions = feedBody.activities.flatMap((activity) => activity.contributions);
    const persisted = uploadBodies.map((body) =>
      contributions.find((contribution) => contribution.id === body.contribution_id),
    );
    check(persisted.every(Boolean), `${size}: contribution feed lists every batch`);
    check(
      persisted.every((contribution) => Array.isArray(contribution.images) && contribution.images.length === 1),
      `${size}: each batch contributes its one render`,
    );
    const monochromeContributions = persisted.filter(
      (contribution) => contribution.render_class === 'monochrome',
    );
    const expectedMonochrome = allBw ? 8 : 3;
    check(monochromeContributions.length === expectedMonochrome, `${size}: the feed names ${expectedMonochrome} monochrome batches`);
    check(
      monochromeContributions.every((contribution) => typeof contribution.deferred_until === 'number'),
      `${size}: every monochrome batch carries its deferred deadline`,
    );
    const activity = page.locator('.contribution-activity');
    await activity.waitFor({ timeout: 60_000 });
    check(await activity.count() === 1, `${size}: history shows one submission card`);
    const primary = await activity.innerText();
    check(!/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/i.test(primary), `${size}: primary history hides internal ids`);
    check(await activity.getByRole('button', { name: 'Withdraw these photos', exact: true }).count() === 1, `${size}: submission has one withdrawal action`);
    await page.screenshot({ path: `${options.out}/contributions-${size}.png`, fullPage: true });
    await activity.getByText('Technical details', { exact: true }).click();
    for (const contribution of persisted) {
      await activity.getByRole('rowheader', { name: contribution.id, exact: true }).waitFor();
    }
    await page.screenshot({ path: `${options.out}/contributions-technical-${size}.png`, fullPage: true });
    await activity.getByText('Technical details', { exact: true }).click();
    const getsBeforeWithdrawal = await page.evaluate(() => window.__looklabWebauthnGets);
    const withdrawalResponse = page.waitForResponse(
      (response) => response.request().method() === 'POST' && new URL(response.url()).pathname === '/api/withdraw',
      { timeout: 60_000 },
    );
    await activity.getByRole('button', { name: 'Withdraw these photos', exact: true }).click();
    const withdrawn = await withdrawalResponse;
    check(withdrawn.ok() && (await withdrawn.json()).status === 'withdrawn', `${size}: submission withdrawal succeeds`);
    check(
      (await page.evaluate(() => window.__looklabWebauthnGets)) - getsBeforeWithdrawal === 1,
      `${size}: withdrawing all photos uses one passkey confirmation`,
    );
    const reloadedFeed = page.waitForResponse(
      (response) => response.request().method() === 'GET' && response.url() === feedResponse.url(),
      { timeout: 60_000 },
    );
    await page.reload();
    const afterWithdrawal = await (await reloadedFeed).json();
    const withdrawnById = new Map(afterWithdrawal.activities.flatMap((entry) => entry.contributions).map((entry) => [entry.id, entry]));
    check(
      uploadBodies.every((entry) => withdrawnById.get(entry.contribution_id)?.status === 'withdrawn'),
      `${size}: withdrawal persists for every submitted photo`,
    );
    await page.locator('.contribution-activity').waitFor({ timeout: 60_000 });
    check(await page.getByRole('button', { name: 'Withdraw these photos', exact: true }).count() === 0, `${size}: withdrawn submission has no active withdrawal action`);
    await page.screenshot({ path: `${options.out}/contributions-withdrawn-${size}.png`, fullPage: true });
    summary.contribution_id = contributionId;
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

async function verifyContributionPayload(size, summary) {
  const log = `${options.out}/verify-payload-${size}.txt`;
  const state = `${options.out}/verify-state-${size}-${randomUUID()}.json`;
  const snapshot = `${options.out}/verification-data-${size}`;
  const response = await fetch(`${options.origin}/data/bodies/${options.body}.json`);
  if (!response.ok) throw new Error(`Cannot load verification camera: HTTP ${response.status}`);
  const { id, make, model, exif_aliases, kind, capabilities, status, mapping_status, schema } = await response.json();
  const entity = { id, make, model, exif_aliases, kind, capabilities, status, mapping_status };
  const filename = id.replace('/', '-');
  await mkdir(`${snapshot}/bodies`, { recursive: true });
  await writeFile(`${snapshot}/bodies/${filename}.body.yaml`, JSON.stringify(entity));
  await writeFile(`${snapshot}/bodies/${filename}.yaml`, schema);
  await cp(`${options.state}/community`, `${snapshot}/community`, { recursive: true });
  const args = [
    'verify-payload',
    '--seeds', summary.seeds,
    '--har', `${options.out}/session-${size}.har`,
    '--module', options['verify-module'],
    '--origin', options.origin,
    '--rp-id', new URL(options.origin).hostname,
    '--data', snapshot,
    '--kind', 'library',
    '--intent', 'native',
    '--monochrome-authorization', 'model-development-180-days',
    ...(summary.declarations ?? []).flatMap((key) => ['--declare-setting', `${key}=0`]),
    '--pow-memory-kib', '8',
    '--pow-iterations', '1',
    '--pow-difficulty', '0',
    '--state', state,
    ...(summary.files ?? [options.raw, options.jpeg]),
  ];
  try {
    const output = execFileSync(options.verifier, args, { cwd: REPO_ROOT, encoding: 'utf8' });
    await writeFile(log, output);
    summary.verification = { log, state, snapshot };
    check(true, `${size}: restored upload passes verify-payload`);
  } catch (error) {
    const output = [error.stdout, error.stderr, error.message].filter(Boolean).map(String).join('\n');
    await writeFile(log, output);
    throw new Error(`${size}: restored upload failed verify-payload; see ${log}`, { cause: error });
  }
}

async function patchHar(path, bodies) {
  const har = JSON.parse(await readFile(path, 'utf8'));
  let patched = 0;
  for (const entry of har.log.entries) {
    const list = bodies.get(`${entry.request.method} ${entry.request.url}`);
    const body = list?.shift();
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
    if (session.authenticator) await session.authenticator.cdp.send('Network.setCacheDisabled', { cacheDisabled: true });
    try {
      const catalog = await (await page.request.get(`${options.origin}/data/index.json`)).json();
      const cameras = catalog.bodies.filter(body => body.kind === 'camera');
      const publishedLooks = catalog.looks.filter(look => (look.visibility === undefined || look.visibility === 'public') && look.versions.length > 0);
      const firstLook = publishedLooks[0];
      check(Boolean(firstLook), `${size}: the catalog publishes a look`);
      const firstLookPattern = new RegExp(`^Simulated: ${firstLook.name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}`);
      const expectedLabels = new Map();
      const expectedKeys = new Map();
      const menuNames = {
        provia: 'PROVIA / Standard', velvia: 'Velvia / Vivid', astia: 'ASTIA / Soft',
        classic_chrome: 'Classic Chrome', classic_negative: 'Classic Negative',
        pro_neg_hi: 'PRO Neg. Hi', pro_neg_std: 'PRO Neg. Std', eterna: 'ETERNA / Cinema',
        eterna_bleach_bypass: 'ETERNA Bleach Bypass', acros: 'ACROS',
        acros_ye: 'ACROS + Ye Filter', acros_r: 'ACROS + R Filter', acros_g: 'ACROS + G Filter',
        monochrome: 'Monochrome', monochrome_ye: 'Monochrome + Ye Filter',
        monochrome_r: 'Monochrome + R Filter', monochrome_g: 'Monochrome + G Filter', sepia: 'Sepia',
      };
      const sampleRequests = [];
      for (const camera of cameras) {
        const bodyPage = await (await page.request.get(`${options.origin}/data/bodies/${camera.id}.json`)).json();
        const card = bodyPage.native_looks.find(look => look.sim_key !== look.look) ?? bodyPage.native_looks[0];
        const label = publishedLooks.find(look => look.slug === card.look)?.name ?? menuNames[card.sim_key] ?? card.sim_key.replace(/_/g, ' ');
        expectedLabels.set(camera.id, label);
        expectedKeys.set(camera.id, card.sim_key);
        sampleRequests.push({
          origin: 'pre_candidate',
          subject: {
            kind: 'body_realization',
            realization: { body: camera.id, epoch: card.epoch, native_sim: card.sim_key },
          },
          capability: 'native_prediction',
          kind: 'missing_independent_captures',
          detail: `Add independent RAW captures with their in-camera JPEGs for ${camera.make} ${camera.model}.`,
          observed_groups: 1,
        });
      }
      sampleRequests.push({
        origin: 'pre_candidate',
        subject: { kind: 'shared_family', family: 'classic-chrome' },
        capability: 'native_prediction',
        kind: 'missing_independent_captures',
        detail: 'Family-wide independent captures are needed.',
        observed_groups: 2,
      });
      await page.route('**/bounties.json', route => route.fulfill({ json: { bounties: [], evidence_requests: sampleRequests } }));
      await page.goto(`${options.origin}/help/passkeys`);
      await page.waitForSelector('h1');
      await page.screenshot({ path: `${options.out}/passkeys-${size}.png`, fullPage: true });

      await page.goto(`${options.origin}/pattern/${seed}`);
      await page.waitForSelector('canvas');
      await page.waitForTimeout(1000);
      await page.screenshot({ path: `${options.out}/pattern-${size}.png` });

      for (const route of ['/', '/looks', '/compare', '/match', '/cameras', '/contribute', '/contribute/wanted', '/me', '/developers', '/developers/pipelines', '/community', '/community/propose', '/community/log', '/review', '/about/how-it-works', '/about/privacy']) {
        await page.goto(`${options.origin}${route}`);
        if (route === '/review') await page.getByText('This area is for community moderators.', { exact: true }).waitFor();
        else await page.waitForSelector('h1');
        check(await page.locator('header.site-header').isVisible(), `${size}: ${route} has the shared shell`);
        check(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth), `${size}: ${route} fits the viewport`);
      }
      await page.goto(`${options.origin}/contribute/wanted`);
      await page.getByRole('heading', { name: 'Where things stand', exact: true }).waitFor();
      await page.locator('#where-things-stand').getByText(sampleRequests[0].detail, { exact: false }).first().waitFor({ timeout: 120_000 });
      for (const camera of cameras) {
        const name = `${camera.make} ${camera.model}`;
        const card = page.getByRole('article').filter({ has: page.getByRole('heading', { name, exact: true }) });
        check(await card.count() === 1, `${size}: where things stand lists ${name}`);
        const text = await card.textContent();
        const label = expectedLabels.get(camera.id);
        const key = expectedKeys.get(camera.id);
        check(text.includes(`Add independent RAW captures with their in-camera JPEGs for ${name}.`), `${size}: where things stand names the open request for ${name}`);
        check(text.includes(`${label}: `), `${size}: where things stand labels the request with the look name for ${name}`);
        if (!label.includes(key)) check(!text.includes(key), `${size}: where things stand hides the raw setting key for ${name}`);
        check(text.includes('1 independent group observed so far.'), `${size}: where things stand shows observed groups for ${name}`);
      }
      const family = page.locator('#where-things-stand .stand-family');
      check(await family.count() === 1, `${size}: family requests are grouped once`);
      const familyText = await family.textContent();
      check(familyText.includes('Family-wide independent captures are needed.'), `${size}: family block names the family request`);
      check(familyText.includes('Classic Chrome'), `${size}: family block names the look`);
      const cameraTexts = await page.locator('#where-things-stand .stand-card').allTextContents();
      check(!cameraTexts.some(text => text.includes('Family-wide independent captures are needed.')), `${size}: family requests are not repeated under cameras`);
      await page.screenshot({ path: `${options.out}/where-things-stand-${size}.png`, fullPage: true });
      await page.unroute('**/bounties.json');
      await page.route('**/bounties.json', route => route.fulfill({ json: { bounties: [], evidence_requests: [] } }));
      await page.goto(`${options.origin}/contribute/wanted`);
      await page.getByText('No open requests right now. You can still share photos you already have.', { exact: true }).waitFor();
      check(await page.getByRole('heading', { name: 'Where things stand', exact: true }).isVisible(), `${size}: where things stand renders with no open requests`);
      await page.unroute('**/bounties.json');
      await page.goto(`${options.origin}/looks`);
      await page.getByRole('button', { name: firstLookPattern }).click();
      await page.locator('main canvas').first().waitFor({ state: 'visible', timeout: 120_000 });
      check(await page.getByRole('slider', { name: 'Comparison divider' }).count() === 0, `${size}: a look opens as a single preview`);
      const overview = page.getByRole('tab', { name: 'Overview', exact: true });
      await overview.focus();
      await page.keyboard.press('ArrowRight');
      check(await page.getByRole('tab', { name: 'Measurements', exact: true }).getAttribute('aria-selected') === 'true', `${size}: arrow key opens measurements`);
      await page.keyboard.press('End');
      check(await page.getByRole('tab', { name: 'History', exact: true }).getAttribute('aria-selected') === 'true', `${size}: End opens history`);
      await page.keyboard.press('Home');
      check(await overview.getAttribute('aria-selected') === 'true', `${size}: Home returns to overview`);
      check(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth), `${size}: look viewer fits the viewport`);
      await page.screenshot({ path: `${options.out}/look-viewer-${size}.png`, fullPage: true });
      const publishedVersion = await page.getByLabel('Published version').locator('option').first().getAttribute('value');
      await page.goto(`${options.origin}/looks/${firstLook.slug}?version=99.0.0`);
      await page.locator('main [role=alert]:visible').first().waitFor();
      check(await page.locator('main canvas:visible').evaluateAll(canvases => canvases.every(canvas => canvas.getContext('2d').getImageData(0, 0, 1, 1).data[3] === 0)), `${size}: an unavailable pinned version never shows the latest preview`);
      await page.getByRole('tab', { name: 'Measurements', exact: true }).click();
      await page.locator('[role=tabpanel] [role=alert]:visible').first().waitFor();
      await page.getByRole('tab', { name: 'History', exact: true }).click();
      await page.getByLabel('Published version').selectOption(publishedVersion);
      await page.waitForURL(url => url.searchParams.get('version') === publishedVersion);
      check(new URL(page.url()).searchParams.get('version') === publishedVersion, `${size}: History version selection updates the shareable URL`);
      await page.getByRole('tab', { name: 'Overview', exact: true }).click();
      await page.locator('main canvas:visible').first().waitFor();
      const publicLooks = catalog.looks.filter(look => look.visibility === undefined || look.visibility === 'public');
      const builtInPublished = cameras.some(camera => camera.looks.some(slug => publishedLooks.some(look => look.slug === slug)));
      for (const theme of ['dark', 'light']) {
        await page.emulateMedia({ colorScheme: theme });
        for (const [name, route] of [['home', '/'], ['looks', '/looks'], ['look', `/looks/${firstLook.slug}`], ['cameras', '/cameras'], ['camera', '/cameras/fujifilm/x-t5']]) {
          await page.goto(`${options.origin}${route}`);
          await page.waitForSelector('h1');
          const imageExpected = !((name === 'cameras' || name === 'camera') && !builtInPublished);
          if (imageExpected) {
            await page.locator('main canvas').first().waitFor({ state: 'visible', timeout: 120_000 });
            for (let top = 0; top < await page.evaluate(() => document.documentElement.scrollHeight); top += viewport.height) {
              await page.evaluate(top => window.scrollTo(0, top), top);
              await page.waitForFunction(() => Array.from(document.querySelectorAll('main canvas')).filter(canvas => {
                const box = canvas.getBoundingClientRect();
                return box.width > 0 && box.height > 0 && box.bottom > 0 && box.top < innerHeight;
              }).every(canvas => canvas.getContext('2d').getImageData(0, 0, 1, 1).data[3] === 255), null, { timeout: 120_000 });
            }
          }
          check(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth), `${size}: ${theme} ${name} fits the viewport after photos load`);
          if (name === 'home' || name === 'looks') {
            check(await page.getByRole('button', { name: /Synthetic Match Case/ }).count() === 0, `${size}: ${name} excludes test fixture looks`);
          }
          if (imageExpected) {
            const glance = await page.evaluate(() => {
              const visible = element => {
                const box = element.getBoundingClientRect();
                return box.width > 0 && box.height > 0;
              };
              const top = selector => {
                const element = Array.from(document.querySelectorAll(selector)).find(visible);
                return element ? element.getBoundingClientRect().top + window.scrollY : null;
              };
              return { image: top('main canvas'), plot: top('main .plot'), table: top('main table') };
            });
            check(glance.image !== null, `${size}: ${theme} ${name} shows an image at the glance level`);
            check(glance.plot === null || glance.plot > glance.image, `${size}: ${theme} ${name} keeps plots below the glance image`);
            check(glance.table === null || glance.table > glance.image, `${size}: ${theme} ${name} keeps tables below the glance image`);
          }
          if (name === 'look' || name === 'camera') {
            await page.getByRole('heading', { name: 'Recipes others use', exact: true }).waitFor({ state: 'visible', timeout: 120_000 });
            check(await page.getByRole('heading', { name: 'Recipes others use', exact: true }).isVisible(), `${size}: ${theme} ${name} shows the recipe leaderboard`);
          }
          if (name === 'home') {
            check(await page.getByRole('group', { name: 'Comparison view', exact: true }).count() === 0, `${size}: ${theme} home hides view-mode controls`);
            check(await page.getByRole('group', { name: 'Image surround', exact: true }).count() === 0, `${size}: ${theme} home hides surround controls`);
            check(await page.getByRole('slider', { name: 'Comparison divider', exact: true }).count() === 1, `${size}: ${theme} home retains the split divider`);
            const index = await (await page.request.get(`${options.origin}/data/index.json`)).json();
            const baseline = index.reference_preview ? 'Neutral' : publicLooks[0].name;
            const initialLook = index.reference_preview ? publicLooks[0] : (publicLooks[1] ?? publicLooks[0]);
            const hero = page.getByRole('figure').filter({ has: page.getByRole('slider', { name: 'Comparison divider', exact: true }) });
            check(await hero.getByRole('img', { name: `Simulated: ${baseline}`, exact: true }).count() > 0, `${size}: ${theme} hero uses its published baseline or the older-index fallback`);
            check(await page.getByRole('button', { name: initialLook.name, exact: true }).getAttribute('aria-pressed') === 'true', `${size}: ${theme} hero starts with the expected right-hand look`);
            const chip = publicLooks.at(-1);
            await page.getByRole('button', { name: chip.name, exact: true }).click();
            await hero.getByRole('img', { name: `Simulated: ${chip.name}`, exact: true }).first().waitFor();
            check(await hero.getByRole('img', { name: `Simulated: ${baseline}`, exact: true }).count() > 0, `${size}: ${theme} switching chips retains the left baseline`);
            await page.getByRole('button', { name: initialLook.name, exact: true }).click();
            await hero.getByRole('img', { name: `Simulated: ${initialLook.name}`, exact: true }).first().waitFor();
          } else if (name === 'look') {
            check(await page.getByRole('group', { name: 'Image surround', exact: true }).count() === 1, `${size}: ${theme} look retains viewer controls`);
          }
          await page.screenshot({ path: `${options.out}/${name}-${theme}-${size}.png`, fullPage: true });
        }
      }
      for (const route of ['/bodies', '/mod', '/log', '/propose', '/passkeys', '/bounties', '/recipes', '/verify']) {
        await page.goto(`${options.origin}${route}`);
        await page.getByRole('heading', { name: 'Not found', exact: true }).waitFor();
      }
      await page.goto(`${options.origin}/`);
      const signIn = page.getByRole('button', { name: 'Sign in', exact: true });
      if (await signIn.count()) {
        await signIn.focus();
        await page.keyboard.press('Enter');
        await page.getByRole('dialog', { name: 'Sign in to looklab' }).waitFor();
        for (let step = 0; step < 8; step += 1) {
          check(await page.evaluate(() => Boolean(document.activeElement.closest('dialog'))), `${size}: Tab keeps focus in the sign-in dialog`);
          await page.keyboard.press('Tab');
        }
        await page.keyboard.press('Escape');
        check(!(await page.getByRole('dialog').isVisible()), `${size}: Escape closes sign-in`);
        check(await signIn.evaluate(element => document.activeElement === element), `${size}: sign-in restores focus`);
      }
      await page.screenshot({ path: `${options.out}/home-${size}.png`, fullPage: true });
      if (options.permalink) {
        await page.goto(`${options.origin}/match/r/${options.permalink}`);
        await page.waitForSelector('h1');
        await page.screenshot({ path: `${options.out}/match-result-${size}.png`, fullPage: true });
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
    await page.setInputFiles('.drop-zone input[type=file]:not([webkitdirectory])', files);
    await page.getByRole('heading', { name: /^\d+ pairs? found$/ }).waitFor({ timeout: 600_000 });
    await page.fill('#body-id', options.body);
    await page.fill('#firmware', options.firmware);
    await page.fill('#sim', options.sim);
    await page.click('summary:has-text("Technical settings editor")');
    if (options.controls) {
      await page.fill('#controls', options.controls.split(',').join('\n'));
    }
    await page.click('summary:has-text("Processing options")');
    await page.selectOption('#workers', String(options.workers));
    await page.click('text=Process locally', { timeout: 600_000 });
    await page.waitForSelector('h2:has-text("Review what")', { timeout: 3_600_000 });
    await page.click('summary:has-text("See exactly")');
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
    const memory = rows.map((row) => Number.parseFloat(row[3]));
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

function validateSnapshot(directory) {
  const output = execFileSync(
    'cargo',
    ['run', '-q', '-p', 'looklab-cli', '--', 'validate-snapshot', directory],
    { cwd: REPO_ROOT, encoding: 'utf8', env: process.env },
  );
  console.log(output.trim());
  check(output.includes('valid snapshot'), `looklab validate-snapshot accepts ${directory}`);
}

async function propose(size) {
  const fixture = `${options.out}/fixture-${size}.raf`;
  await writeFile(fixture, `looklab fixture donation for ${size}\n`);
  const session = await open(VIEWPORTS[size]);
  const { page, problems } = session;
  try {
    if (session.authenticator) await session.authenticator.cdp.send('Network.setCacheDisabled', { cacheDisabled: true });
    await page.goto(`${options.origin}/review`);
    await page.getByText('This area is for community moderators.', { exact: true }).waitFor();
    check(await page.locator('header').getByRole('link', { name: /Review/ }).count() === 0, `${size}: visitor header has no review entry`);
    check(await page.locator('#mod-open-session').count() === 0, `${size}: visitor cannot open a moderator session`);
    await page.screenshot({ path: `${options.out}/review-visitor-${size}.png`, fullPage: true });
    await page.goto(`${options.origin}/community/propose`);
    await page.waitForSelector('#kind', { timeout: 60_000 });
    await page.getByRole('button', { name: 'Sign in', exact: true }).first().click();
    const profileRequestPromise = page.waitForRequest((request) => new URL(request.url()).pathname === '/api/session', { timeout: 60_000 });
    await page.getByRole('button', { name: 'Create a profile', exact: true }).click();
    const profileRequest = await profileRequestPromise;
    const profileCredential = profileRequest.postDataJSON().credential_id;
    await page.locator('dialog').waitFor({ state: 'hidden', timeout: 60_000 });
    await page.selectOption('#kind', 'body');
    await page.fill('#body-make', 'Fujifilm');
    await page.fill('#body-model', 'X-E5');
    await page.selectOption('#body-mapping', 'fixture_backed');
    await page.setInputFiles('section[aria-labelledby="evidence-heading"] input[type=file]:not([webkitdirectory])', [fixture]);
    await page.waitForSelector('#donate-0', { timeout: 60_000 });
    await page.check('#donate-0');
    await page.screenshot({ path: `${options.out}/propose-${size}.png`, fullPage: true });
    await page.emulateMedia({ colorScheme: 'dark' });
    await page.screenshot({ path: `${options.out}/propose-${size}-dark.png`, fullPage: true });
    await page.emulateMedia({ colorScheme: 'light' });
    check(await page.isChecked('#donate-0'), `${options.browser} ${size}: fixture consent is per file`);

    await page.click('#submit-proposal');
    await page.waitForSelector('#donation-confirm', { timeout: 60_000 });
    await page.screenshot({ path: `${options.out}/propose-confirm-${size}.png`, fullPage: true });
    check(
      (await page.locator('#donation-confirm').textContent()).includes(`fixture-${size}.raf`),
      `${options.browser} ${size}: the donation confirmation names the exact file`,
    );
    await page.click('#confirm-donation');
    await page.waitForSelector('#submitted-link', { timeout: 300_000 });
    const href = await page.locator('#submitted-link').getAttribute('href');
    check(Boolean(href), `${options.browser} ${size}: submission returned a proposal`);
    const id = href.split('/').pop();
    await page.click('#submitted-link');
    await page.waitForSelector('text=Evidence', { timeout: 60_000 });
    await page.screenshot({ path: `${options.out}/proposal-${size}.png`, fullPage: true });

    const grant = await page.request.post(`${options.origin}/api/__stub/moderator/${profileCredential}`);
    check(grant.ok(), `${size}: stub grants moderator to the test profile`);
    await page.locator('.account-menu summary').click();
    await page.getByRole('button', { name: 'Sign out', exact: true }).click();
    await page.getByRole('button', { name: 'Sign in', exact: true }).first().click();
    await page.getByRole('button', { name: 'Continue with passkey', exact: true }).click();
    await page.locator('dialog').waitFor({ state: 'hidden', timeout: 60_000 });
    await page.goto(`${options.origin}/review`);
    await page.waitForSelector('#mod-open-session', { timeout: 60_000 });
    await page.route('**/api/mod/session', async (route) => {
      const response = await route.fetch();
      const reply = await response.json();
      await route.fulfill({ response, json: { ...reply, expires_in_seconds: 1 } });
    });
    await page.click('#mod-open-session');
    await page.waitForSelector('#mod-queue', { timeout: 60_000 });
    await page.waitForSelector('#mod-open-session', { timeout: 10_000 });
    check(await page.locator('#mod-queue').count() === 0, `${size}: expired moderator session returns to confirmation`);
    await page.unroute('**/api/mod/session');
    await page.click('#mod-open-session');
    await page.waitForSelector('#mod-queue', { timeout: 60_000 });
    await page.locator('[aria-label="Proposal queue, use j and k to navigate"]').focus();
    await page.keyboard.press('j');
    await page.waitForSelector('#mod-proposal', { timeout: 60_000 });
    await page.keyboard.press('k');
    await page.waitForSelector('#mod-proposal', { timeout: 60_000 });
    if (await page.locator('.review-back').isVisible()) await page.locator('.review-back').click();
    await page.click(`button[data-proposal="${id}"]`);
    await page.waitForSelector('#mod-proposal', { timeout: 60_000 });
    await page.screenshot({ path: `${options.out}/moderate-${size}.png`, fullPage: true });
    await page.emulateMedia({ colorScheme: 'dark' });
    await page.screenshot({ path: `${options.out}/moderate-${size}-dark.png`, fullPage: true });
    await page.emulateMedia({ colorScheme: 'light' });
    await page.focus('#mod-approve');
    await page.keyboard.press('Enter');
    await page.fill('#mod-reason', 'The payload validates and the donated fixture matches it.');
    check((await page.locator('#mod-log-preview').textContent()).includes(id), `${size}: decision preview identifies the proposal`);
    check((await page.locator('#mod-log-preview').textContent()).includes('approved'), `${size}: decision preview describes the decision`);
    await page.screenshot({ path: `${options.out}/decision-preview-${size}.png`, fullPage: true });
    await page.focus('#mod-confirm-decision');
    await page.keyboard.press('Enter');
    await page.waitForSelector('#mod-decision', { timeout: 300_000 });
    const version = await page.getAttribute('#mod-decision', 'data-snapshot-version');
    check(Boolean(version), `${options.browser} ${size}: approval wrote snapshot v${version}`);
    await page.screenshot({ path: `${options.out}/decision-${size}.png`, fullPage: true });
    await page.goto(`${options.origin}/community/log`);
    await page.locator('.activity-list').filter({ hasText: id }).first().waitFor({ timeout: 60_000 });
    check((await page.locator('main').textContent()).includes('The payload validates and the donated fixture matches it.'), `${size}: the public log contains the decision reason`);
    for (const colorScheme of ['light', 'dark']) {
      await page.emulateMedia({ colorScheme });
      for (const route of ['community', 'community/log']) {
        await page.goto(`${options.origin}/${route}`);
        await page.waitForSelector('.activity-list');
        if (route === 'community') {
          const activity = page.locator('.activity-list');
          check(!/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/i.test(await activity.textContent()), `${size}: recent activity does not expose internal ids`);
          check(!/\d{4}-\d{2}-\d{2}T/.test(await activity.textContent()), `${size}: recent activity uses readable local dates`);
          check(await activity.getByRole('link', { name: /A moderator approved a proposal/ }).count() > 0, `${size}: decisions link to their proposals`);
        }
        await page.screenshot({ path: `${options.out}/${route.replaceAll('/', '-')}-${size}-${colorScheme}.png`, fullPage: true });
        check(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), `${route} ${size} ${colorScheme}: no horizontal overflow`);
      }
    }

    const snapshotDir = `${options.state}/community/v${version}`;
    const exported = await readFile(`${snapshotDir}/fixtures/donated/fixture-${size}.raf`, 'utf8');
    check(
      exported === `looklab fixture donation for ${size}\n`,
      `${options.browser} ${size}: the approved snapshot exports the donated file`,
    );
    const manifest = await readFile(`${snapshotDir}/fixtures/manifest.toml`, 'utf8');
    check(
      !manifest.includes('source ='),
      `${options.browser} ${size}: the published manifest drops the private blob source`,
    );
    await writeFile(`${options.out}/proposal-errors-${size}.json`, JSON.stringify(problems, null, 2));
    check(problems.length === 0, `${options.browser} ${size}: no page errors during the proposal flow`);
    return { id, version };
  } finally {
    await close(session);
  }
}

async function propose_edges(size) {
  const session = await open(VIEWPORTS[size]);
  const { page, problems } = session;
  try {
    await page.goto(`${options.origin}/community/propose?kind=lut_look#from-a-preset`);
    await page.waitForSelector('#from-a-preset[open]', { timeout: 60_000 });
    const presetVisible = await page.evaluate(() => {
      const guide = document.querySelector('#from-a-preset');
      if (!guide) return false;
      const rect = guide.getBoundingClientRect();
      return rect.top < innerHeight && rect.bottom > 0;
    });
    check(presetVisible, `${options.browser} ${size}: the preset deep link opens and scrolls to the guide`);
    await page.goto(`${options.origin}/community/propose?kind=lut_look`);
    await page.waitForSelector('#lut-name', { timeout: 60_000 });
    const lutOrder = await page.evaluate(() => {
      const section = document.querySelector('section[aria-labelledby="lut-heading"]');
      const zone = section?.querySelector('section.drop-zone');
      const fields = [...(section?.querySelectorAll('input, select, textarea') ?? [])].filter((element) => !zone?.contains(element));
      return Boolean(zone && fields.length > 0 && fields.every((element) => zone.compareDocumentPosition(element) & Node.DOCUMENT_POSITION_FOLLOWING));
    });
    check(lutOrder, `${options.browser} ${size}: the custom-look drop zone precedes the name and other fields`);
    check(await page.locator('section[aria-labelledby="evidence-heading"]').count() === 0, `${options.browser} ${size}: a custom look shows no donation section`);
    await page.click('#submit-proposal');
    await page.waitForSelector('section[aria-labelledby="submit"] .error', { timeout: 60_000 });
    check((await page.locator('#payload-yaml').inputValue()).trim() === '', `${options.browser} ${size}: a missing .cube produces a form error, not an empty payload`);
    await page.selectOption('#lut-encoding', 'display');
    await page.waitForSelector('#lut-base-look', { timeout: 60_000 });
    const index = await (await page.request.get(`${options.origin}/data/index.json`)).json();
    const bases = index.looks
      .filter((look) => (look.visibility ?? 'public') === 'public')
      .flatMap((look) => look.versions.filter((version) =>
        ['published', 'rooted'].includes(version.status)
        && (version.applicability?.state ?? 'current') !== 'invalidated'
        && version.support !== 'early_estimate'
        && ((version.matchable ?? true) || version.artifact === 'source-lut.json')
      ).map((version) => `${look.slug}@${version.version}`));
    const neutral = bases.some((base) => base.startsWith('looklab-neutral-srgb-v1@'));
    const expected = bases.length + (neutral ? 0 : 1);
    const offered = await page.locator('#lut-base-look option:not([value=""])').count();
    check(offered === expected, `${options.browser} ${size}: display bases offer ${expected} importer-supported versions`);

    await page.goto(`${options.origin}/community/propose?kind=interface_profile`);
    await page.waitForSelector('section[aria-labelledby="interface-heading"]', { timeout: 60_000 });
    check(await page.locator('section[aria-labelledby="evidence-heading"]').count() === 0, `${options.browser} ${size}: an editing-app profile shows no donation section`);
    await page.setInputFiles('section[aria-labelledby="interface-heading"] input[type=file]', `${REPO_ROOT}/data/interfaces/example-app-lut3d-v1.yaml`);
    await page.waitForSelector('section[aria-labelledby="interface-heading"] p.muted', { timeout: 60_000 });
    const interfaceOrder = await page.evaluate(() => {
      const section = document.querySelector('section[aria-labelledby="interface-heading"]');
      const zone = section?.querySelector('section.drop-zone');
      const identity = [...(section?.querySelectorAll('p.muted') ?? [])].find((element) => element.textContent.includes('example-app/lut3d-v1'));
      const outsideFields = [...(section?.querySelectorAll('input, select, textarea') ?? [])].filter((element) => !zone?.contains(element));
      return Boolean(zone && identity && outsideFields.length === 0 && zone.compareDocumentPosition(identity) & Node.DOCUMENT_POSITION_FOLLOWING);
    });
    check(interfaceOrder, `${options.browser} ${size}: the editing-app profile drop zone precedes the profile identity and other fields`);

    await page.goto(`${options.origin}/community/propose?kind=mapping`);
    await page.waitForSelector('#mapping-camera', { timeout: 60_000 });
    await page.selectOption('#mapping-camera', 'fujifilm/x-s10');
    await page.getByLabel('Correct when an existing simulation became available').check();
    await page.waitForSelector('#mapping-existing', { timeout: 60_000 });
    await page.selectOption('#mapping-existing', { index: 1 });
    const sourceEpoch = await page.locator('#mapping-existing option').nth(1).getAttribute('data-epoch');
    await page.selectOption('#mapping-epoch', sourceEpoch);
    check((await page.locator('#payload-yaml').inputValue()).trim() === '', `${options.browser} ${size}: correcting to the source firmware range produces no payload`);

    await page.goto(`${options.origin}/community/propose?kind=schema_change`);
    await page.waitForSelector('#schema-camera', { timeout: 60_000 });
    check(await page.locator('section[aria-labelledby="evidence-heading"]').count() === 0, `${options.browser} ${size}: a control change shows no donation section`);
    await page.selectOption('#schema-camera', 'fujifilm/x-s10');
    await page.waitForSelector('#schema-epoch', { timeout: 60_000 });
    check((await page.locator('fieldset input[type=checkbox]').count()) > 0, `${options.browser} ${size}: controls are an explicit multi-select`);
    await page.getByLabel(/dynamic_range/).check();
    await page.waitForSelector('#edit-dynamic_range-default', { timeout: 60_000 });
    check(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), `${options.browser} ${size}: the existing-control editor fits the viewport`);
    await page.fill('#edit-dynamic_range-default', 'dr200');
    await page.getByRole('button', { name: 'Add this change', exact: true }).click();
    check((await page.locator('#payload-yaml').inputValue()).includes('dr200'), `${options.browser} ${size}: an existing-control edit reaches the payload`);
    await page.locator('details.disclosure', { hasText: 'Add a control this camera does not list' }).locator('summary').click();
    await page.fill('#add-key', 'phone_test_control');
    await page.selectOption('#add-type', 'int');
    await page.fill('#add-min', '0');
    await page.fill('#add-max', '1');
    await page.fill('#add-step', '1');
    await page.fill('#add-default', '0');
    await page.click('#add-control');
    check((await page.locator('#payload-yaml').inputValue()).includes('phone_test_control'), `${options.browser} ${size}: the separate add-control draft reaches the payload`);
    check(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), `${options.browser} ${size}: the add-control form fits the viewport`);
    await page.selectOption('#schema-camera', '');
    await page.waitForSelector('#schema-epoch', { state: 'detached', timeout: 60_000 });
    check((await page.locator('#payload-yaml').inputValue()).trim() === '', `${options.browser} ${size}: clearing the camera clears the control payload`);

    await page.goto(`${options.origin}/community/propose?kind=body`);
    await page.waitForSelector('#body-kind', { timeout: 60_000 });
    check(await page.locator('section[aria-labelledby="evidence-heading"]').count() === 1, `${options.browser} ${size}: a camera body shows the donation section`);
    await page.selectOption('#body-kind', 'software');
    await page.waitForSelector('section[aria-labelledby="evidence-heading"]', { state: 'detached', timeout: 60_000 });
    check(await page.locator('section[aria-labelledby="evidence-heading"]').count() === 0, `${options.browser} ${size}: a software body shows no donation section`);
    await page.screenshot({ path: `${options.out}/propose-edges-${size}.png`, fullPage: true });
    await writeFile(`${options.out}/propose-edges-errors-${size}.json`, JSON.stringify(problems, null, 2));
    check(problems.length === 0, `${options.browser} ${size}: no page errors during proposal edge flows`);
    return { bases: expected };
  } finally {
    await close(session);
  }
}

async function developers() {
  const session = await open(VIEWPORTS.desktop);
  const { page } = session;
  try {
    const cdp = await page.context().newCDPSession(page);
    await cdp.send('Network.setCacheDisabled', { cacheDisabled: true });
    await page.goto(`${options.origin}/developers`);
    await page.waitForSelector('#card-inspector input[type=file]', { state: 'attached', timeout: 120_000 });
    const vectors = `${REPO_ROOT}/data/card/conformance-v1`;
    const index = JSON.parse(await readFile(`${vectors}/index.json`, 'utf8'));
    for (const entry of index.files) {
      await page.locator('#card-inspector input[type=file]').setInputFiles(`${vectors}/${entry.path}`);
      await page.waitForFunction((name) => document.querySelector('#card-inspector .inspection-result p')?.textContent === name, entry.path.split('/').pop());
      const actual = await page.locator('[data-card-outcome]').getAttribute('data-card-outcome');
      check(actual === entry.outcome, `card ${entry.path}: ${entry.outcome}`);
    }
    await page.locator('#render-tag-checker summary').click();
    const tags = JSON.parse(await readFile(`${vectors}/render-tags.json`, 'utf8'));
    for (const vector of tags) {
      await page.locator('#render-tag-text').fill(vector.input);
      await page.getByRole('button', { name: 'Check render tag', exact: true }).click();
      await page.waitForSelector('[data-tag-outcome]:not([data-tag-outcome=checking])');
      const actual = await page.locator('[data-tag-outcome]').getAttribute('data-tag-outcome');
      check(vector.result.error ? actual !== 'valid' : actual === 'valid', `render tag ${vector.input}`);
      if (!vector.result.error) {
        const output = await page.locator('[data-tag-outcome]').textContent();
        for (const value of Object.values(vector.result)) check(output.includes(value), `render tag field ${value}`);
      }
    }
    await page.evaluate(() => {
      const original = File.prototype.arrayBuffer;
      globalThis.restoreFileReads = () => { File.prototype.arrayBuffer = original; };
      File.prototype.arrayBuffer = function () {
        if (!this.name.startsWith('delayed-')) return original.call(this);
        return new Promise((resolve, reject) => {
          globalThis.releaseFileRead = async () => {
            try { resolve(await original.call(this)); } catch (error) { reject(error); }
            await new Promise(done => requestAnimationFrame(() => requestAnimationFrame(done)));
          };
        });
      };
    });
    await page.locator('#card-inspector input[type=file]').setInputFiles({
      name: 'delayed-card.llk', mimeType: 'application/octet-stream',
      buffer: await readFile(`${vectors}/valid/card-33.llk`),
    });
    await page.waitForFunction(() => typeof globalThis.releaseFileRead === 'function');
    await page.locator('#card-inspector input[type=file]').setInputFiles(`${vectors}/malformed/bad-magic.llk`);
    await page.waitForSelector('[data-card-outcome=rejected]');
    await page.evaluate(() => globalThis.releaseFileRead());
    check(await page.locator('[data-card-outcome]').getAttribute('data-card-outcome') === 'rejected', 'Late card read cannot replace the latest selection');
    await page.evaluate(() => { globalThis.releaseFileRead = null; });
    await page.locator('#render-tag-checker input[type=file]').setInputFiles({
      name: 'delayed-photo.jpg', mimeType: 'image/jpeg', buffer: Buffer.from([0xff, 0xd8, 0xff, 0xd9]),
    });
    await page.waitForFunction(() => typeof globalThis.releaseFileRead === 'function');
    await page.locator('#render-tag-text').fill('looklab-card/1 malformed');
    await page.getByRole('button', { name: 'Check render tag', exact: true }).click();
    await page.waitForSelector('[data-tag-outcome=malformed]');
    await page.evaluate(() => globalThis.releaseFileRead());
    check(await page.locator('[data-tag-outcome]').getAttribute('data-tag-outcome') === 'malformed', 'Late JPEG read cannot replace the latest render-tag input');
    await page.evaluate(() => globalThis.restoreFileReads());
    const [download] = await Promise.all([
      page.waitForEvent('download'),
      page.getByRole('link', { name: 'Download conformance vectors', exact: false }).click(),
    ]);
    await page.getByText('Download SHA-256', { exact: true }).click();
    const expectedArchiveHash = (await page.locator('.conformance-sha256').textContent()).trim();
    const { createHash } = await import('node:crypto');
    const archiveBytes = await readFile(await download.path());
    check(createHash('sha256').update(archiveBytes).digest('hex') === expectedArchiveHash, 'Downloaded conformance ZIP matches its published SHA-256');
    for (const [size, viewport] of Object.entries(VIEWPORTS)) {
      await page.setViewportSize(viewport);
      for (const colorScheme of ['dark', 'light']) {
        await page.emulateMedia({ colorScheme });
        for (const route of ['developers', 'developers/pipelines', 'about/how-it-works']) {
          await page.goto(`${options.origin}/${route}`);
          await page.waitForSelector('article.developers-page');
          check(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), `${route}: no horizontal overflow at ${size}`);
          await page.screenshot({ path: `${options.out}/${route.replaceAll('/', '-')}-${size}-${colorScheme}.png`, fullPage: true });
        }
      }
    }
    const verificationPath = '**/api/cards/verification';
    await page.route(verificationPath, route => route.fulfill({
      json: { verifications: [{ firmware_id: 'test-firmware', firmware_version: '1.2.0', look: 'conformance-tone', look_version: '1.0.0', images: 24, median_de: 1.5, p95_de: 3.2 }] },
    }));
    for (const [size, viewport] of Object.entries(VIEWPORTS)) {
      await page.setViewportSize(viewport);
      await page.goto(`${options.origin}/developers`);
      await page.getByRole('rowheader', { name: 'test-firmware' }).waitFor();
      const listing = page.locator('#firmware-listings');
      check((await listing.textContent()).includes('1.2.0'), `${size}: firmware version appears`);
      check((await listing.textContent()).includes('24'), `${size}: verified photo count appears`);
      check((await listing.textContent()).includes('Nearly identical'), `${size}: measured closeness appears`);
      await listing.getByText('Measurements', { exact: true }).click();
      check((await listing.textContent()).includes('1.50'), `${size}: median measurement appears`);
      check((await listing.textContent()).includes('3.20'), `${size}: p95 measurement appears`);
      check(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), `${size}: firmware table stays within page width`);
      await page.screenshot({ path: `${options.out}/developers-firmware-${size}.png`, fullPage: true });
    }
    await page.unroute(verificationPath);
    await page.route(verificationPath, route => route.fulfill({ json: { verifications: [] } }));
    await page.goto(`${options.origin}/developers`);
    await page.getByRole('heading', { name: 'No firmware listed yet', exact: true }).waitFor();
    await page.unroute(verificationPath);
    await page.route(verificationPath, route => route.fulfill({ status: 503, body: 'Unavailable' }));
    await page.goto(`${options.origin}/developers`);
    await page.locator('#firmware-listings [role=alert]').waitFor();
    check((await page.locator('#firmware-listings').textContent()).includes('Could not load listed firmware'), 'Unavailable firmware service shows an error, not an empty list');
    await page.unroute(verificationPath);
    return { cards: index.files.length, tags: tags.length };
  } finally {
    await close(session);
  }
}

function wasmRequests(page) {
  return page.evaluate(() => performance.getEntriesByType('resource')
    .filter(entry => new URL(entry.name).pathname.endsWith('.wasm'))
    .map(entry => ({ path: new URL(entry.name).pathname, start: entry.startTime })));
}

const chunkName = path => path.split('/').pop();

async function lazy() {
  const session = await open(VIEWPORTS.desktop);
  const { page } = session;
  try {
    await page.addInitScript(() => {
      performance.setResourceTimingBufferSize(2000);
      const timings = { canvases: [] };
      window.looklabLazyChunks = timings;
      const put = CanvasRenderingContext2D.prototype.putImageData;
      CanvasRenderingContext2D.prototype.putImageData = function (...args) {
        const result = put.apply(this, args);
        const canvas = this.canvas;
        const entry = { width: canvas.width, height: canvas.height, paint: null };
        requestAnimationFrame(() => requestAnimationFrame(() => {
          if (canvas.closest('.viewer') && canvas.getBoundingClientRect().width > 0) {
            entry.paint = performance.now();
            timings.canvases.push(entry);
          }
        }));
        return result;
      };
    });
    await page.goto(`${options.origin}/`, { waitUntil: 'domcontentloaded' });
    await page.waitForFunction(() => window.looklabLazyChunks.canvases.some(entry => entry.paint !== null), null, { timeout: 120_000 });
    const firstPhoto = await page.evaluate(() => Math.min(...window.looklabLazyChunks.canvases.filter(entry => entry.paint !== null).map(entry => entry.paint)));
    const stems = await page.evaluate(async () => {
      const stylesheet = document.querySelector('link[rel="stylesheet"][href]');
      const prefix = new URL(stylesheet.href).pathname.replace(/[^/]*$/, '');
      const response = await fetch(`${prefix}pkg/__wasm_split_manifest.json`, { cache: 'no-store' });
      if (!response.ok) throw new Error(`split manifest request failed with ${response.status}`);
      const manifest = await response.json();
      return [...new Set(Object.values(manifest).flat())];
    });
    check(stems.length > 0, `split manifest lists ${stems.length} chunk files`);
    const chunkFiles = new Set(stems.map(stem => `${stem}.wasm`));
    const chunkRequests = requests => requests.filter(entry => chunkFiles.has(chunkName(entry.path)));
    const painted = chunkRequests(await wasmRequests(page));
    check(painted.every(entry => entry.start >= firstPhoto), 'no route chunk is requested before the first photo paints');
    await page.waitForFunction((expected) => {
      const seen = new Set(performance.getEntriesByType('resource')
        .filter(entry => new URL(entry.name).pathname.endsWith('.wasm'))
        .map(entry => new URL(entry.name).pathname.split('/').pop().replace(/\.wasm$/, '')));
      return expected.every(stem => seen.has(stem));
    }, stems, { timeout: 120_000 });
    await page.waitForTimeout(1000);
    const idle = chunkRequests(await wasmRequests(page));
    for (const stem of stems) {
      const count = idle.filter(entry => chunkName(entry.path) === `${stem}.wasm`).length;
      check(count === 1, `idle prefetch requests ${stem} exactly once`);
    }
    const requestCount = idle.length;
    const navigate = async (route, heading) => {
      await page.evaluate((route) => {
        const stylesheet = document.querySelector('link[rel="stylesheet"][href]');
        const prefix = new URL(stylesheet.href).pathname.replace(/[^/]*$/, '');
        history.pushState({}, '', prefix + route.replace(/^\//, ''));
        dispatchEvent(new PopStateEvent('popstate', { state: history.state }));
      }, route);
      await page.getByRole('heading', { name: heading, exact: true }).waitFor({ timeout: 120_000 });
      await page.waitForTimeout(500);
      check(chunkRequests(await wasmRequests(page)).length === requestCount, `${route} navigation after prefetch requests no route chunk`);
    };
    await navigate('/contribute', 'Help build looks');
    await navigate('/review', 'Review queue');
    await navigate('/developers', 'Make your firmware load looklab looks');
    const direct = [];
    for (const [route, heading] of [['/contribute', 'Help build looks'], ['/review', 'Review queue']]) {
      const visit = await open(VIEWPORTS.desktop);
      const { page: directPage } = visit;
      try {
        await directPage.addInitScript(() => performance.setResourceTimingBufferSize(2000));
        await directPage.goto(`${options.origin}${route}`, { waitUntil: 'domcontentloaded' });
        await directPage.getByRole('heading', { name: heading, exact: true }).waitFor({ timeout: 120_000 });
        const rendered = await directPage.evaluate(() => performance.now());
        const requests = chunkRequests(await wasmRequests(directPage));
        check(requests.some(entry => entry.start < rendered), `${route} direct visit requests its route chunk before the page renders`);
        direct.push({ route, chunks: requests.map(entry => chunkName(entry.path)) });
      } finally {
        await close(visit);
      }
    }
    return { chunks: stems.length, requests: requestCount, direct };
  } finally {
    await close(session);
  }
}

async function compareViews() {
  const session = await open(VIEWPORTS.desktop);
  const { page } = session;
  const timing = [];
  try {
    if (options.browser === 'chromium') {
      const cdp = await page.context().newCDPSession(page);
      await cdp.send('Network.setCacheDisabled', { cacheDisabled: true });
    }
    for (const [size, viewport] of Object.entries(VIEWPORTS)) {
      await page.setViewportSize(viewport);
      for (const colorScheme of ['dark', 'light']) {
        await page.emulateMedia({ colorScheme });
        await page.goto(`${options.origin}/compare?a=sunlit-negative&b=cold-steel`);
        const viewer = page.locator('.compare-page > .viewer');
        await viewer.waitFor({ timeout: 120_000 });
        const divider = viewer.getByRole('slider', { name: 'Comparison divider' });
        await divider.focus();
        await page.keyboard.press('Home');
        check(await divider.getAttribute('aria-valuenow') === '0', 'Compare divider Home');
        await page.keyboard.press('ArrowRight');
        check(await divider.getAttribute('aria-valuenow') === '1', 'Compare divider arrow key');
        await page.keyboard.press('End');
        check(await divider.getAttribute('aria-valuenow') === '100', 'Compare divider End');
        await page.keyboard.press('Home');
        const frame = await viewer.locator('.viewer-images').boundingBox();
        await page.mouse.move(frame.x + 1, frame.y + frame.height / 2);
        await page.mouse.down();
        await page.mouse.move(frame.x + frame.width / 2, frame.y + frame.height / 2);
        await page.mouse.up();
        check(Math.abs(Number(await divider.getAttribute('aria-valuenow')) - 50) < 3, 'Compare mouse drag');
        if (options.browser === 'chromium') {
          const cdp = await page.context().newCDPSession(page);
          const handle = await divider.boundingBox();
          const start = { x: handle.x + handle.width / 2, y: handle.y + handle.height / 2 };
          await cdp.send('Input.dispatchTouchEvent', { type: 'touchStart', touchPoints: [start] });
          await cdp.send('Input.dispatchTouchEvent', { type: 'touchMove', touchPoints: [{ x: frame.x + frame.width * .7, y: start.y }] });
          await cdp.send('Input.dispatchTouchEvent', { type: 'touchEnd', touchPoints: [] });
          check(Math.abs(Number(await divider.getAttribute('aria-valuenow')) - 70) < 3, 'Compare touch drag');
        }
        for (const [label, value] of [['Split', 'split'], ['Side by side', 'side-by-side'], ['Toggle', 'toggle'], ['Difference', 'difference']]) {
          await viewer.getByRole('button', { name: label, exact: true }).focus();
          await page.keyboard.press('Enter');
          await page.waitForFunction(view => new URL(location.href).searchParams.get('view') === view, value);
          check(new URL(page.url()).searchParams.get('view') === value, `Compare ${label} URL`);
          if (value === 'toggle') {
            await viewer.getByRole('button', { name: 'Switch look', exact: true }).click();
            check(await viewer.getByRole('img', { name: 'Simulated: Cold Steel', exact: true }).isVisible(), 'Compare toggle click shows second look');
            const hold = viewer.getByRole('button', { name: 'Hold to flip looks' });
            await hold.focus();
            await page.keyboard.down('Space');
            check(await viewer.getByRole('img', { name: 'Simulated: Sunlit Negative', exact: true }).isVisible(), 'Compare toggle hold flips the clicked look');
            await page.keyboard.up('Space');
            check(await viewer.getByRole('img', { name: 'Simulated: Cold Steel', exact: true }).isVisible(), 'Compare toggle release restores the clicked look');
          }
          if (value === 'difference') {
            check(await page.getByLabel('Difference legend').isVisible(), 'Compare difference legend');
            await viewer.locator('.difference-timing').filter({ hasText: 'Difference computed' }).waitFor({ timeout: 120_000 });
            timing.push({ size, colorScheme, text: await viewer.locator('.difference-timing').textContent() });
          }
          await page.getByRole('region', { name: 'Comparison summary', exact: true }).waitFor({ timeout: 120_000 });
          check(await page.locator('.compare-breakdown-row').count() === 5, 'Compare five-row summary');
          if (await page.getByText('Not enough measured data to rate this difference yet.', { exact: false }).count()) {
            check(await page.locator('.compare-summary meter').count() === 0, 'Compare unsupported data has no rating bars');
            check(await page.locator('.compare-summary .closeness').count() === 0, 'Compare unsupported data has no closeness label');
          }
          check(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), `Compare ${size} fits viewport`);
          await page.screenshot({ path: `${options.out}/compare-${value}-${size}-${colorScheme}.png`, fullPage: true });
          await page.reload();
          await page.locator('.compare-page > .viewer').waitFor({ timeout: 120_000 });
          check(await page.locator('.compare-page > .viewer').getByRole('button', { name: label, exact: true }).getAttribute('aria-pressed') === 'true', `Compare ${label} reload`);
        }
      }
    }
    return timing;
  } catch (error) {
    await page.screenshot({ path: `${options.out}/compare-failure.png`, fullPage: true });
    throw error;
  } finally {
    await close(session);
  }
}

async function account() {
  if (options.browser !== 'chromium') {
    throw new Error('the account flow needs the Chromium CDP authenticator; use --browser chromium');
  }
  const session = await open(VIEWPORTS.desktop);
  const { page, context, browser: engineBrowser, problems, expectedMissingResources } = session;
  const { cdp, authenticatorId } = session.authenticator;
  const signInDialog = page.getByRole('dialog', { name: 'Sign in to looklab', exact: true });
  const accountMenu = page.getByRole('button', { name: 'River Photographer', exact: true });
  await cdp.send('Network.setCacheDisabled', { cacheDisabled: true });
  let second;
  try {
    await context.grantPermissions(['clipboard-read', 'clipboard-write'], { origin: options.origin });
    await page.goto(`${options.origin}/me`);
    await page.getByRole('button', { name: 'Sign in', exact: true }).first().focus();
    await page.keyboard.press('Enter');
    await signInDialog.waitFor();
    await page.keyboard.press('Escape');
    check(await signInDialog.count() === 0, 'Escape closes sign-in dialog');
    await page.getByRole('button', { name: 'Sign in', exact: true }).first().click();
    for (const [size, viewport] of Object.entries(VIEWPORTS)) {
      await page.setViewportSize(viewport);
      for (const colorScheme of ['light', 'dark']) {
        await page.emulateMedia({ colorScheme });
        await page.getByRole('button', { name: 'Close dialog' }).focus();
        const focusOrder = [
          page.getByRole('button', { name: 'Continue with passkey', exact: true }),
          page.getByLabel('Display name (optional)'),
          page.getByRole('button', { name: 'Create a profile', exact: true }),
          page.getByRole('link', { name: 'How this works', exact: true }),
          page.getByRole('button', { name: 'Close dialog', exact: true }),
        ];
        for (const target of focusOrder) {
          await page.keyboard.press('Tab');
          check(await target.evaluate((element) => element === document.activeElement), 'Sign-in keyboard order follows returning sign-in, new profile, help, then Close');
        }
        check(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), `Sign-in dialog ${size} ${colorScheme}: no horizontal scroll`);
        await page.screenshot({ path: `${options.out}/account-dialog-${size}-${colorScheme}.png`, fullPage: true });
      }
    }
    await page.getByLabel('Display name (optional)').fill('River Photographer');
    const created = page.waitForResponse((response) => new URL(response.url()).pathname === '/api/session');
    const activation = page.waitForRequest((request) => request.method() === 'POST' && new URL(request.url()).pathname === '/api/session');
    await page.getByRole('button', { name: 'Create a profile', exact: true }).click();
    const firstProfile = await (await created).json();
    const activationRequest = (await activation).postDataJSON();
    check(Array.isArray(activationRequest.pow) && activationRequest.pow.length === 16, 'New profile activation sends the server-required work proof');
    await signInDialog.waitFor({ state: 'hidden' });
    check(await accountMenu.isVisible(), 'Optional display name reaches account menu');
    await accountMenu.focus();
    await page.keyboard.press('Enter');
    check(await accountMenu.getAttribute('aria-expanded') === 'true', 'Enter opens the account menu');
    await page.getByRole('link', { name: 'Your contributions', exact: true }).focus();
    await page.keyboard.press('Escape');
    check(await accountMenu.getAttribute('aria-expanded') === 'false', 'Escape closes the account menu');
    check(await accountMenu.evaluate((element) => element === document.activeElement), 'Escape returns focus to the account menu button');
    await page.keyboard.press('Space');
    check(await accountMenu.getAttribute('aria-expanded') === 'true', 'Space opens the account menu');
    await page.keyboard.press('Escape');
    await accountMenu.click();
    check(await page.getByRole('link', { name: 'Review', exact: true }).count() === 0, 'Ordinary profile has no Review link');
    await page.getByRole('button', { name: 'Sign out', exact: true }).click();
    await page.goto(`${options.origin}/status/${firstProfile.status_token}`);
    await page.waitForURL(`${options.origin}/me`);
    await page.getByRole('button', { name: 'Private status link', exact: true }).waitFor();
    check(await accountMenu.isVisible(), 'Private status link signs this browser in and opens /me');
    check(await page.getByRole('dialog', { name: 'Private status link', exact: true }).count() === 0, 'Private link details stay closed until requested');
    check(await page.locator('main a[href$="/help/passkeys"]').count() === 0, 'Signed-in /me does not repeat sign-in help');
    await page.getByRole('button', { name: 'Private status link', exact: true }).click();
    const linkDialog = page.getByRole('dialog', { name: 'Private status link', exact: true });
    await linkDialog.waitFor();
    check(page.url() === `${options.origin}/me`, 'Private link dialog opens without leaving /me');
    const privateLink = await linkDialog.getByLabel('Private status link').inputValue();
    check(privateLink === `${options.origin}/status/${firstProfile.status_token}`, 'Private link dialog shows the full capability URL');
    await linkDialog.getByRole('button', { name: 'Copy link', exact: true }).click();
    const copied = await page.waitForFunction((expected) => navigator.clipboard.readText().then((text) => text === expected), privateLink).then(() => true, () => false);
    check(copied, 'Copy link puts the private URL on the clipboard');
    await page.keyboard.press('Escape');
    await linkDialog.waitFor({ state: 'hidden' });
    check(page.url() === `${options.origin}/me`, 'Closing the private link dialog leaves /me open');
    check(await page.getByRole('heading', { name: 'Your contributions', exact: true }).isVisible(), 'Closing the private link dialog leaves /me usable');
    const rssLink = page.getByRole('link', { name: 'Subscribe with RSS', exact: true });
    const rssHref = await rssLink.getAttribute('href');
    const rssResponse = page.waitForResponse((response) => new URL(response.url()).pathname === `/status/${firstProfile.status_token}.rss`).catch(() => null);
    const rssDownload = page.waitForEvent('download').catch(() => null);
    await rssLink.evaluate((anchor) => anchor.click());
    check((await Promise.race([rssResponse, rssDownload])) !== null, 'RSS link navigates to the feed instead of the SPA status route');
    const feed = await page.request.get(new URL(rssHref, options.origin).href);
    check(feed.status() === 200, 'RSS feed responds');
    check((feed.headers()['content-type'] ?? '').startsWith('application/rss+xml'), 'RSS feed uses the feed media type');
    check((await feed.text()).includes('<rss version="2.0">'), 'RSS feed returns feed data');
    await page.goto(`${options.origin}/me`);
    await page.getByRole('button', { name: 'Private status link', exact: true }).waitFor();
    const independent = await engineBrowser.newContext({ viewport: VIEWPORTS.mobile });
    const independentPage = await independent.newPage();
    const independentFeed = independentPage.waitForResponse((response) => new URL(response.url()).pathname === `/status/${firstProfile.status_token}` && (response.headers()['content-type'] ?? '').startsWith('application/json'));
    await independentPage.goto(privateLink);
    await independentPage.waitForURL(`${options.origin}/me`);
    await independentPage.getByRole('heading', { name: 'Your contributions', exact: true }).waitFor();
    check(await independentPage.locator('.account-menu summary').isVisible(), 'Copied private link signs in a separate browser');
    check((await independentFeed).ok(), 'Copied private link loads the contribution feed in a separate browser');
    await independent.close();
    await accountMenu.click();
    await page.getByRole('button', { name: 'Sign out', exact: true }).click();
    await page.getByRole('button', { name: 'Sign in', exact: true }).first().click();
    const resumed = page.waitForResponse((response) => new URL(response.url()).pathname === '/api/session');
    await page.getByRole('button', { name: 'Continue with passkey', exact: true }).click();
    const resumedProfile = await (await resumed).json();
    await signInDialog.waitFor({ state: 'hidden' });
    check((await page.request.get(`${options.origin}/status/${firstProfile.status_token}`)).ok(), 'Local sign-out preserves existing status link');
    const credentials = await cdp.send('WebAuthn.getCredentials', { authenticatorId });
    const credential = credentials.credentials[0];
    check(Boolean(credential), 'Profile stores a discoverable passkey');
    second = await engineBrowser.newContext({ viewport: VIEWPORTS.mobile });
    const other = await second.newPage();
    const otherCdp = await second.newCDPSession(other);
    await otherCdp.send('Network.setCacheDisabled', { cacheDisabled: true });
    await otherCdp.send('WebAuthn.enable');
    await otherCdp.send('Emulation.setFocusEmulationEnabled', { enabled: true });
    const otherAuthenticator = await otherCdp.send('WebAuthn.addVirtualAuthenticator', { options: { protocol: 'ctap2', transport: 'internal', hasResidentKey: true, hasUserVerification: true, isUserVerified: true, automaticPresenceSimulation: true } });
    await otherCdp.send('WebAuthn.addCredential', { authenticatorId: otherAuthenticator.authenticatorId, credential });
    await other.goto(`${options.origin}/me`);
    await other.getByRole('button', { name: 'Sign in', exact: true }).first().click();
    const otherResponse = other.waitForResponse((response) => new URL(response.url()).pathname === '/api/session');
    await other.getByRole('button', { name: 'Continue with passkey', exact: true }).click();
    const otherProfile = await (await otherResponse).json();
    await other.getByRole('dialog', { name: 'Sign in to looklab', exact: true }).waitFor({ state: 'hidden' });
    check((await page.request.get(`${options.origin}/status/${resumedProfile.status_token}`)).ok(), 'Second browser leaves first session signed in');
    check((await other.request.get(`${options.origin}/status/${otherProfile.status_token}`)).ok(), 'Second browser has its own valid session');
    // Virtual authenticators do not share signature counters.
    const updatedCredentials = await otherCdp.send('WebAuthn.getCredentials', { authenticatorId: otherAuthenticator.authenticatorId });
    await cdp.send('WebAuthn.removeCredential', { authenticatorId, credentialId: credential.credentialId });
    await cdp.send('WebAuthn.addCredential', { authenticatorId, credential: updatedCredentials.credentials[0] });
    const credentialId = Buffer.from(credential.credentialId, 'base64').toString('base64url');
    check((await page.request.post(`${options.origin}/api/__stub/moderator/${credentialId}`)).ok(), 'Stub moderator grant succeeds');
    await page.reload();
    await accountMenu.waitFor();
    await accountMenu.click();
    check(await page.getByRole('link', { name: 'Review', exact: true }).isVisible(), 'Reload refreshes newly granted roles without another passkey prompt');
    await accountMenu.click();
    await accountMenu.click();
    await page.getByRole('button', { name: 'Sign out', exact: true }).click();
    await page.getByRole('button', { name: 'Sign in', exact: true }).first().click();
    await page.getByRole('button', { name: 'Continue with passkey', exact: true }).click();
    await signInDialog.waitFor({ state: 'hidden' });
    await accountMenu.click();
    check(await page.getByRole('link', { name: 'Review', exact: true }).isVisible(), 'Moderator sees Review in account menu');
    for (const [size, viewport] of Object.entries(VIEWPORTS)) {
      await page.setViewportSize(viewport);
      for (const colorScheme of ['light', 'dark']) {
        await page.emulateMedia({ colorScheme });
        for (const route of ['me', 'help/passkeys', 'about/privacy']) {
          await page.goto(`${options.origin}/${route}`);
          await page.locator('h1').waitFor();
          check(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), `${route} ${size} ${colorScheme}: no horizontal scroll`);
          await page.screenshot({ path: `${options.out}/account-${route.replaceAll('/', '-')}-${size}-${colorScheme}.png`, fullPage: true });
        }
      }
    }
    await accountMenu.click();
    await page.getByRole('button', { name: 'Sign out everywhere', exact: true }).click();
    await page.getByRole('button', { name: 'Sign in', exact: true }).first().waitFor();
    check((await other.request.get(`${options.origin}/status/${otherProfile.status_token}`)).status() === 404, 'Sign out everywhere revokes second browser');
    check((await page.request.get(`${options.origin}/status/${resumedProfile.status_token}`)).status() === 404, 'Sign out everywhere revokes first browser');
    const revokedLink = `${options.origin}/status/${firstProfile.status_token}`;
    const restoreUrl = `${options.origin}/api/session`;
    expectedMissingResources.add(restoreUrl);
    const revokedRestore = page.waitForResponse((response) => response.url() === restoreUrl && response.request().method() === 'GET');
    await page.goto(revokedLink);
    check((await revokedRestore).status() === 404, 'Revoked capability cannot restore a session');
    await page.getByRole('alert').waitFor();
    check(page.url() === `${options.origin}/status/${firstProfile.status_token}`, 'Revoked status link does not sign the browser in');
    check(await page.getByRole('button', { name: 'Sign in', exact: true }).first().isVisible(), 'Revoked status link leaves the browser signed out');
    await other.reload();
    await other.getByRole('button', { name: 'Sign in', exact: true }).first().waitFor();
    check(problems.length === 0, `Account browser console: ${problems.join('; ')}`);
    return { created: true, signedInAgain: true, independentSessions: true, revokedEverywhere: true, moderatorMenu: true };
  } finally {
    await second?.close();
    await close(session);
  }
}

const result = { browser: options.browser, out: options.out };
if (options.flow === 'all' || options.flow === 'match') {
  result.permalink = await match('desktop');
  if (process.argv.includes('--expected') && options.expected) {
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
  await verifyContributionPayload('desktop', result.contribute);
  result.contribute_mobile = await contribute('mobile');
  await verifyContributionPayload('mobile', result.contribute_mobile);
}
if (options.flow === 'all-bw') {
  options['bw-all'] = true;
  result.contribute = await contribute('desktop');
  await verifyContributionPayload('desktop', result.contribute);
  result.contribute_mobile = await contribute('mobile');
  await verifyContributionPayload('mobile', result.contribute_mobile);
}
if (options.flow === 'all' || options.flow === 'pages') {
  if (!options.permalink && result.permalink) {
    options.permalink = Buffer.from(result.permalink, 'utf8').toString('base64url');
  }
  await pages();
}
if (options.flow === 'all' || options.flow === 'propose') {
  result.proposals = [await propose('desktop'), await propose('mobile')];
  for (const entry of result.proposals) {
    validateSnapshot(`${options.state}/community/v${entry.version}`);
  }
  result.propose_edges = [await propose_edges('desktop'), await propose_edges('mobile')];
}
if (options.flow === 'flatness') {
  result.flatness = await flatness();
}
if (options.flow === 'all' || options.flow === 'developers') {
  result.developers = await developers();
}
if (options.flow === 'all' || options.flow === 'lazy') {
  result.lazy = await lazy();
}
console.log(JSON.stringify(result, null, 2));
if (options.flow === 'compare') {
  console.log(JSON.stringify({ compare: await compareViews() }, null, 2));
}
if (options.flow === 'account') {
  console.log(JSON.stringify({ account: await account() }, null, 2));
}
